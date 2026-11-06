//! Agent hook installation and execution.

use std::{
    io::{self, Read},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use jiff::Timestamp;
use tokio::time::timeout;
use yappr::{
    choices::{self, QuestionStatus},
    entry::{self, Draft},
    peers,
    store::Store,
};

use super::{
    args::{CommonArgs, HookArgs, HookScope, HooksCommand},
    questions::final_state,
    shown,
    writer::deliver_now,
    Usage,
};

/// The settings file of `agent` for `scope`. CLAUDE_CONFIG_DIR and
/// CODEX_HOME move the user settings, as they do for the agents.
fn hook_settings(agent: yappr::hooks::Agent, scope: HookScope) -> Result<PathBuf> {
    use yappr::hooks::Agent;
    let home = || dirs::home_dir().context("cannot find the home directory");
    let env_dir = |var: &str| std::env::var_os(var).map(PathBuf::from);
    Ok(match (agent, scope) {
        (Agent::Claude, HookScope::User) => env_dir("CLAUDE_CONFIG_DIR")
            .map_or_else(|| home().map(|h| h.join(".claude")), Ok)?
            .join("settings.json"),
        (Agent::Claude, HookScope::Project) => PathBuf::from(".claude/settings.json"),
        (Agent::Claude, HookScope::Local) => PathBuf::from(".claude/settings.local.json"),
        (Agent::Codex, HookScope::User) => env_dir("CODEX_HOME")
            .map_or_else(|| home().map(|h| h.join(".codex")), Ok)?
            .join("hooks.json"),
        (Agent::Codex, HookScope::Project) => PathBuf::from(".codex/hooks.json"),
        (Agent::Codex, HookScope::Local) => {
            return Err(
                Usage("Codex has no local settings. Use --scope user or project".into()).into(),
            )
        }
    })
}

/// The settings at `path`, or an empty object if the file does not exist.
fn read_settings(path: &std::path::Path) -> Result<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON. Fix it first", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn write_settings(path: &std::path::Path, settings: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

/// The device that answers when an install names none: the one allowed
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
                "several devices can answer: {}. Choose one with --to",
                names.join(", ")
            ))
            .into())
        }
    }
}

pub(super) fn hooks_command(common: &CommonArgs, command: HooksCommand) -> Result<()> {
    use yappr::hooks::{self, Agent};
    match command {
        HooksCommand::Install {
            agent,
            to,
            events,
            scope,
            timeout,
        } => {
            let path = hook_settings(agent, scope)?;
            let events = match events {
                Some(text) => {
                    hooks::parse_events(agent, &text).map_err(|e| Usage(format!("{e:#}")))?
                }
                None => hooks::default_events(agent),
            };
            let store = common.store()?;
            let device = match to {
                Some(device) => device,
                None => default_device(&store)?,
            };
            let id = peers::resolve(store.dir(), &device)?.id;
            if !store.allow_list().contains(&id) {
                bail!("{device} cannot answer yet. Allow it first: yappr allow {device}");
            }
            let program = std::env::current_exe()?.to_string_lossy().into_owned();
            let dir = std::fs::canonicalize(store.dir())?
                .to_string_lossy()
                .into_owned();
            let secs = timeout.as_secs().max(1);
            let mut settings = read_settings(&path)?;
            if path.exists() {
                let backup = path.with_file_name(format!(
                    "{}.yappr-backup",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
                std::fs::copy(&path, &backup)?;
            }
            hooks::install(
                &mut settings,
                &events,
                |_| hooks::command(&program, &dir, agent, Some(&device), secs),
                secs,
            )?;
            write_settings(&path, &settings)?;
            eprintln!("Added yappr hooks to {}:", path.display());
            for selected in &events {
                let what = if selected.event.asks() {
                    format!("ask {device}, then allow or deny")
                } else {
                    "post an entry".into()
                };
                let matcher = selected
                    .matcher
                    .as_deref()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default();
                let commands = if selected.commands.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", selected.commands.join(" or "))
                };
                eprintln!("  {}{matcher}{commands}: {what}", selected.event.key());
            }
            if agent == Agent::Codex {
                eprintln!("In Codex, run /hooks once and trust the yappr hooks.");
            }
            eprintln!("To remove them: yappr hooks remove {agent}");
            Ok(())
        }
        HooksCommand::Status { agent } => {
            let agents = agent.map_or(vec![Agent::Claude, Agent::Codex], |a| vec![a]);
            let mut found = false;
            for agent in agents {
                for scope in [HookScope::User, HookScope::Project, HookScope::Local] {
                    let Ok(path) = hook_settings(agent, scope) else {
                        continue;
                    };
                    let Ok(settings) = read_settings(&path) else {
                        continue;
                    };
                    let installed = hooks::installed(&settings);
                    if installed.is_empty() {
                        continue;
                    }
                    found = true;
                    println!("{agent} ({}):", path.display());
                    for (event, command) in installed {
                        println!("  {event}: {command}");
                    }
                }
            }
            if !found {
                eprintln!("No yappr hooks are installed. Run \"yappr hooks install claude\" or \"yappr hooks install codex\".");
            }
            Ok(())
        }
        HooksCommand::Remove { agent, scope } => {
            let path = hook_settings(agent, scope)?;
            let mut settings = read_settings(&path)?;
            if hooks::remove(&mut settings) {
                write_settings(&path, &settings)?;
                eprintln!("Removed the yappr hooks from {}.", path.display());
            } else {
                eprintln!("{} has no yappr hooks.", path.display());
            }
            Ok(())
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
    let action = match args.events {
        Some(events) => hooks::plan_selected(
            args.agent,
            &input,
            &hooks::parse_events(args.agent, &events)?,
        ),
        None => hooks::plan(args.agent, &input),
    };
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
            let to = args
                .to
                .context("no device to ask. Run: yappr hooks install")?;
            let responder = peers::resolve(store.dir(), &to)?.id;
            if !store.allow_list().contains(&responder) {
                bail!("{to} cannot answer. Allow it first: yappr allow {to}");
            }
            let expires_at =
                Timestamp::now().checked_add(jiff::SignedDuration::try_from(args.timeout)?)?;
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
            let state = match timeout(args.timeout, final_state(common, &question.id)).await {
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
