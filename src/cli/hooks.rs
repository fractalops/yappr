//! Agent hook execution. The agent plugins run `yappr hook` for each event.

use std::{
    io::{self, Read},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use jiff::Timestamp;
use yappr::{
    choices::{self, QuestionStatus},
    config::{FeedConfig, HookSettings, HookWhen},
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
    // Where yappr cannot tell, such as over SSH, the user can be at the
    // terminal. Act only when the user is surely away.
    let watch = settings.when == HookWhen::Away;
    if watch && super::presence::presence() != super::presence::Presence::Away {
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
                subject: Some(subject),
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
        Action::Activity {
            title,
            step,
            detail,
            subject,
        } => {
            let entry =
                store.append(activity(&store, args.agent, title, step, detail, subject)?)?;
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
                subject: Some(subject),
                ..Default::default()
            };
            let question = choices::create_with(&store, question, details)?;
            // The question expires at the deadline, and a decision cannot
            // arrive after it. Read the state once more, in case a decision
            // reached disk at the deadline.
            let here = || super::presence::presence() == super::presence::Presence::Here;
            let answer = final_state(common, &question.id);
            let state = match wait_away(answer, settings.wait, watch.then_some(here), RECHECK).await
            {
                Wait::Done(state) => state,
                // The user is back. Stop the request on the device, and let
                // the agent ask in the terminal.
                Wait::Back => {
                    choices::cancel(&store, &question.id).ok();
                    deliver_later(
                        &store,
                        args.agent,
                        store.entries()?.last().map_or(0, |e| e.id),
                    )?;
                    return Ok(None);
                }
                Wait::TimedOut => store
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

/// How often a request checks whether the user is back.
const RECHECK: Duration = Duration::from_secs(3);

/// The end of a wait for an answer.
#[derive(Debug, PartialEq)]
enum Wait<T> {
    Done(T),
    TimedOut,
    /// The user came back to the computer before the answer.
    Back,
}

/// Wait up to `limit` for `answer`. With `here`, stop when it says that the
/// user is back, checked each `every`.
async fn wait_away<T>(
    answer: impl std::future::Future<Output = T>,
    limit: Duration,
    here: Option<impl Fn() -> bool>,
    every: Duration,
) -> Wait<T> {
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(answer, deadline);
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    loop {
        tokio::select! {
            value = &mut answer => return Wait::Done(value),
            () = &mut deadline => return Wait::TimedOut,
            _ = ticks.tick(), if here.is_some() => {
                if here.as_ref().is_some_and(|here| here()) {
                    return Wait::Back;
                }
            }
        }
    }
}

/// The most recent steps on an activity card.
const STEPS: usize = 5;

/// The next activity card of `agent` for `subject`. A step adds to the steps
/// of the last card, and a card without a step starts again.
fn activity(
    store: &Store,
    agent: agent_hooks::Agent,
    title: String,
    step: Option<String>,
    detail: Option<String>,
    subject: String,
) -> Result<Draft> {
    let mut steps: Vec<serde_json::Value> = Vec::new();
    let mut data = serde_json::Map::new();
    let detail = if let Some(step) = step {
        let last = store.entries()?.into_iter().rev().find(|e| {
            e.kind == "state" && e.source == agent.name() && e.subject.as_deref() == Some(&subject)
        });
        let last = last.as_ref().and_then(|e| e.extra.get("activity"));
        if let Some(serde_json::Value::Array(old)) = last.and_then(|d| d.get("steps")) {
            steps.clone_from(old);
        }
        steps.push(step.into());
        let extra = steps.len().saturating_sub(STEPS);
        steps.drain(..extra);
        let result = detail.or_else(|| {
            last.and_then(|d| d.get("command_result"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
        if let Some(result) = result.as_ref().filter(|s| !s.is_empty()) {
            data.insert("command_result".into(), result.clone().into());
        }
        result
    } else {
        detail
    };
    let mut body = steps
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(|s| format!("- {s}"))
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(detail) = detail.filter(|s| !s.is_empty()) {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&detail);
    }
    data.insert("steps".into(), steps.into());
    Ok(Draft {
        source: agent.name().into(),
        kind: "state".into(),
        level: None,
        subject: Some(subject),
        title: Some(title),
        body,
        format: Some("markdown".into()),
        data: None,
        extra: [("activity".into(), data.into())].into_iter().collect(),
    })
}

/// The hook settings of one run. A flag comes first, then the environment
/// that the plugin sets, then `yappr config`.
struct Settings {
    when: HookWhen,
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
        let when = match env_setting("YAPPR_HOOK_WHEN") {
            Some(text) => <HookWhen as clap::ValueEnum>::from_str(&text, true)
                .map_err(|_| anyhow::anyhow!("YAPPR_HOOK_WHEN: use away or always"))?,
            None => saved.when.unwrap_or_default(),
        };
        Ok(Self {
            when,
            to,
            wait: wait.min(agent_hooks::MAX_WAIT),
            events,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_stops_waiting_when_the_user_comes_back() {
        let every = Duration::from_millis(20);
        let checks = std::cell::Cell::new(0);
        let back_on_third = || {
            checks.set(checks.get() + 1);
            checks.get() == 3
        };
        let never = std::future::pending::<()>();
        let wait = wait_away(never, Duration::from_secs(120), Some(back_on_third), every).await;
        assert_eq!(wait, Wait::Back);
        assert_eq!(checks.get(), 3);

        let away = || false;
        let never = std::future::pending::<()>();
        let wait = wait_away(never, every * 5, Some(away), every).await;
        assert_eq!(wait, Wait::TimedOut);

        let none: Option<fn() -> bool> = None;
        let answer = async { 7 };
        assert_eq!(wait_away(answer, every, none, every).await, Wait::Done(7));
    }

    #[test]
    fn activity_keeps_the_last_command_output_until_the_turn_ends() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let add = |step: Option<&str>, detail: Option<&str>| {
            let draft = activity(
                &store,
                agent_hooks::Agent::Claude,
                "Claude activity".into(),
                step.map(str::to_owned),
                detail.map(str::to_owned),
                "claude/project".into(),
            )
            .unwrap();
            store.append(draft).unwrap()
        };

        add(Some("command"), Some("Last result:\n```\nOutput\n```"));
        for step in ["file a", "file b", "file c", "file d"] {
            add(Some(step), None);
        }
        let entry = add(Some("file e"), None);
        assert!(entry.data.is_none());
        assert_eq!(
            entry.extra["activity"]["steps"].as_array().unwrap().len(),
            5
        );
        assert_eq!(
            entry.body,
            "- file a\n- file b\n- file c\n- file d\n- file e\n\nLast result:\n```\nOutput\n```"
        );
        let entry = add(Some("quiet command"), Some(""));
        assert!(!entry.body.contains("Last result"));
        assert_eq!(add(None, Some("Done.")).body, "Done.");
        assert_eq!(add(Some("next turn"), None).body, "- next turn");
    }
}
