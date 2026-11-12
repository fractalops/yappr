//! Agent hook execution. The agent plugins run `yappr hook` for each event.

use std::{
    io::{self, Read},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use jiff::Timestamp;
use tokio::time::timeout;
use yappr::{
    choices::{self, QuestionStatus},
    config::{FeedConfig, HookSettings},
    entry::{self, Draft},
    hooks::{self as agent_hooks, Selected},
    peers,
    store::Store,
};

use super::{
    args::{parse_interval, CommonArgs, HookArgs},
    questions::final_state,
    shown,
    writer::deliver_now,
    Usage,
};

/// The device that answers when the settings name none: the one allowed
/// device that receives this feed, or else the one allowed device.
fn default_device(store: &Store) -> Result<String> {
    let allowed = store.allow_list().load()?;
    let peers: Vec<(String, peers::Peer)> = peers::list(store.dir())?
        .into_iter()
        .filter(|(_, peer)| allowed.contains(&peer.id))
        .collect();
    let receivers: Vec<&String> = peers
        .iter()
        .filter(|(_, peer)| peer.receives)
        .map(|(alias, _)| alias)
        .collect();
    match (receivers.as_slice(), peers.as_slice()) {
        ([one], _) => Ok((*one).clone()),
        ([], [(one, _)]) => Ok(one.clone()),
        ([], []) => Err(Usage(
            "no device can answer. Pair one first with: yappr start --pair".into(),
        )
        .into()),
        _ => {
            let names: Vec<&str> = peers.iter().map(|(alias, _)| alias.as_str()).collect();
            Err(Usage(format!(
                "several devices can answer: {}. Choose one with: yappr config --hook-to DEVICE",
                names.join(", ")
            ))
            .into())
        }
    }
}

/// Handle one hook event. A failure never blocks the agent: yappr reports
/// it on stderr and gives no decision, so the agent asks as before.
pub(super) async fn hook(common: &CommonArgs, args: HookArgs) {
    if let Some(through) = args.deliver {
        if let Ok(store) = common.store() {
            deliver_now(common, &store, through).await.ok();
        }
        return;
    }
    match run_hook(common, args).await {
        Ok(Some(decision)) => println!("{decision}"),
        Ok(None) => {}
        Err(error) => eprintln!("yappr hook: {}", shown(&error)),
    }
}

/// Start a process that delivers the feed through `through`, and do not wait
/// for it. The saved entry stays in the feed if the delivery fails.
fn deliver_later(store: &Store, agent: yappr::hooks::Agent, through: u64) -> Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("--dir")
        .arg(store.dir())
        .args(["hook", "--agent", agent.name(), "--deliver"])
        .arg(through.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // A process group of its own, so that the agent does not stop it with
    // the hook.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().context("start the delivery")?;
    Ok(())
}

async fn run_hook(common: &CommonArgs, args: HookArgs) -> Result<Option<serde_json::Value>> {
    use yappr::hooks::{self, Action};
    let mut text = String::new();
    io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut text)
        .context("read the hook event")?;
    let input: hooks::Input =
        serde_json::from_str(&text).context("the hook event is not valid JSON")?;
    let saved = FeedConfig::load(&common.dir()?)?.hooks;
    let settings = Settings::resolve(&args, &saved)?;
    let action = hooks::plan_selected(args.agent, &input, &settings.events);
    if action == Action::Nothing {
        return Ok(None);
    }
    let store = common.store()?;
    match action {
        Action::Nothing => Ok(None),
        Action::Post {
            kind,
            title,
            body,
            subject,
        } => {
            let entry = store.append(Draft {
                source: args.agent.name().into(),
                kind,
                level: None,
                subject,
                title: Some(title),
                body,
                format: None,
                data: None,
                extra: Default::default(),
            })?;
            // The agent waits for the hook. A device that is not reachable
            // must not stop the agent, so deliver in another process.
            deliver_later(&store, args.agent, entry.id)?;
            Ok(None)
        }
        Action::Ask {
            event,
            title,
            body,
            subject,
        } => {
            let to = match settings.to {
                Some(to) => to,
                None => default_device(&store)?,
            };
            let responder = peers::resolve(store.dir(), &to)?.id;
            if !store.allow_list().contains(&responder) {
                bail!("{to} cannot answer. Allow it first: yappr allow {to}");
            }
            let expires_at =
                Timestamp::now().checked_add(jiff::SignedDuration::try_from(settings.wait)?)?;
            let question = choices::new_question(
                title,
                body,
                args.agent.name().into(),
                choices::approval_options(),
                responder,
                Some(expires_at),
                true,
            );
            let details = entry::Details {
                subject,
                ..Default::default()
            };
            let question = choices::create_with(&store, question, details)?;
            // The question expires at the deadline, and a decision cannot
            // arrive after it. Read the state once more, in case a decision
            // reached disk at the deadline.
            let state = match timeout(settings.wait, final_state(common, &question.id)).await {
                Ok(state) => state,
                Err(_) => store
                    .entries()
                    .and_then(|entries| choices::get(&entries, &question.id)),
            };
            match state {
                Ok(state) if state.status == QuestionStatus::Answered => {
                    let approved = state.decision.is_some_and(|d| d.value == "approve");
                    Ok(Some(hooks::decision(args.agent, event, approved)))
                }
                Ok(_) => Ok(None),
                Err(error) => {
                    choices::cancel(&store, &question.id).ok();
                    Err(error)
                }
            }
        }
    }
}

/// The hook settings of one run. A flag comes first, then the environment
/// that the plugin sets, then `yappr config`.
struct Settings {
    to: Option<String>,
    wait: Duration,
    events: Vec<Selected>,
}

/// The value of an environment variable, with an empty value as unset. The
/// plugins pass unset options as empty values.
fn env_setting(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

impl Settings {
    fn resolve(args: &HookArgs, saved: &HookSettings) -> Result<Self> {
        let to = args
            .to
            .clone()
            .or_else(|| env_setting("YAPPR_HOOK_TO"))
            .or_else(|| saved.to.clone());
        let wait = match (args.timeout, env_setting("YAPPR_HOOK_TIMEOUT")) {
            (Some(wait), _) => wait,
            (None, Some(text)) => {
                parse_interval(&text).map_err(|e| anyhow::anyhow!("YAPPR_HOOK_TIMEOUT: {e}"))?
            }
            (None, None) => saved
                .timeout
                .map_or(agent_hooks::DEFAULT_WAIT, Duration::from_secs),
        };
        let events = match args
            .events
            .clone()
            .or_else(|| env_setting("YAPPR_HOOK_EVENTS"))
            .or_else(|| saved.events.clone())
        {
            Some(text) => agent_hooks::parse_events(&text)?,
            None => agent_hooks::default_events(),
        };
        Ok(Self {
            to,
            wait: wait.min(agent_hooks::MAX_WAIT),
            events,
        })
    }
}
