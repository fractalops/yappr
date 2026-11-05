//! Hooks for coding agents: Claude Code and Codex run `yappr hook` for an
//! event, such as a permission request, and yappr asks a device or posts an
//! entry.
//!
//! [`plan`] turns the event JSON that the agent sends on stdin into an
//! [`Action`]. [`decision`] is the JSON that the agent reads back. [`install`]
//! and [`remove`] change the hook entries in a settings file and keep the
//! entries of other tools.
use std::fmt;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};

/// A coding agent that runs hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    /// The name in entries and messages.
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The hook events that yappr can handle, by their names in the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The agent asks for permission to use a tool. yappr asks the device.
    PermissionRequest,
    /// Before each use of a matching tool. yappr asks the device. Opt-in.
    PreToolUse,
    /// A tool completed. yappr posts its result. Opt-in.
    PostToolUse,
    /// A tool failed. Claude Code only. Opt-in.
    PostToolUseFailure,
    /// The agent needs input. Claude Code only.
    Notification,
    /// The agent finished its turn.
    Stop,
    SessionStart,
    SessionEnd,
    SubagentStop,
}

impl Event {
    pub fn key(self) -> &'static str {
        match self {
            Event::PermissionRequest => "PermissionRequest",
            Event::PreToolUse => "PreToolUse",
            Event::PostToolUse => "PostToolUse",
            Event::PostToolUseFailure => "PostToolUseFailure",
            Event::Notification => "Notification",
            Event::Stop => "Stop",
            Event::SessionStart => "SessionStart",
            Event::SessionEnd => "SessionEnd",
            Event::SubagentStop => "SubagentStop",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        ALL.into_iter().find(|e| e.key() == key)
    }

    /// True if the hook waits for an answer from a device.
    pub fn asks(self) -> bool {
        matches!(self, Event::PermissionRequest | Event::PreToolUse)
    }

    fn supported(self, agent: Agent) -> bool {
        !(agent == Agent::Codex && matches!(self, Event::Notification | Event::PostToolUseFailure))
    }

    fn filters_commands(self) -> bool {
        matches!(
            self,
            Event::PreToolUse | Event::PostToolUse | Event::PostToolUseFailure
        )
    }
}

const ALL: [Event; 9] = [
    Event::PermissionRequest,
    Event::PreToolUse,
    Event::PostToolUse,
    Event::PostToolUseFailure,
    Event::Notification,
    Event::Stop,
    Event::SessionStart,
    Event::SessionEnd,
    Event::SubagentStop,
];

/// The events of an install without --events.
pub fn default_events(agent: Agent) -> Vec<Selected> {
    // A Claude notification repeats a permission request or a stop, so it is
    // not a default.
    [Event::PermissionRequest, Event::Stop]
        .into_iter()
        .filter(|e| e.supported(agent))
        .map(|event| Selected {
            event,
            matcher: None,
            commands: Vec::new(),
        })
        .collect()
}

/// An event to install, with its agent matcher and command patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub event: Event,
    pub matcher: Option<String>,
    /// Any pattern may match. An empty list accepts every command.
    pub commands: Vec<String>,
}

impl Selected {
    /// The event selection saved in the installed hook command.
    pub fn selector(&self) -> String {
        let mut prefix = self.event.key().to_string();
        if let Some(matcher) = &self.matcher {
            prefix.push(':');
            prefix.push_str(matcher);
        }
        if self.commands.is_empty() {
            return prefix;
        }
        self.commands
            .iter()
            .map(|pattern| format!("{prefix}:{pattern}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Parse `event[:matcher[:command-pattern]]` entries separated by commas.
pub fn parse_events(agent: Agent, text: &str) -> Result<Vec<Selected>> {
    let mut out: Vec<Selected> = Vec::new();
    for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, matcher, pattern) = match item.split_once(':') {
            Some((name, rest)) => {
                let (matcher, pattern) = split_matcher(rest);
                (name.trim(), Some(matcher.trim()), pattern.map(str::trim))
            }
            None => (item, None, None),
        };
        if matcher == Some("") || pattern == Some("") {
            bail!("use event:matcher:command-pattern with no empty fields");
        }
        let events: Vec<Event> = match name.to_ascii_lowercase().as_str() {
            "permission" | "permissionrequest" => vec![Event::PermissionRequest],
            "pretooluse" => vec![Event::PreToolUse],
            "posttooluse" => vec![Event::PostToolUse],
            "posttoolusefailure" => vec![Event::PostToolUseFailure],
            "notification" => vec![Event::Notification],
            "stop" => vec![Event::Stop],
            "session" => vec![Event::SessionStart, Event::SessionEnd],
            "sessionstart" => vec![Event::SessionStart],
            "sessionend" => vec![Event::SessionEnd],
            "subagent" | "subagentstop" => vec![Event::SubagentStop],
            other => bail!(
                "unknown event {other:?}. Use permission, pretooluse:TOOL, posttooluse, posttoolusefailure, notification, stop, session, or subagent"
            ),
        };
        for event in events {
            if !event.supported(agent) {
                bail!("{agent} has no {} event", event.key());
            }
            if event == Event::PreToolUse && matcher.is_none() {
                bail!("give the tools for pretooluse, such as pretooluse:Bash");
            }
            if let Some(pattern) = pattern {
                if !event.filters_commands() {
                    bail!("command patterns apply only to pretooluse, posttooluse, and posttoolusefailure");
                }
                command_regex(pattern)?;
            }
            merge_selection(
                &mut out,
                Selected {
                    event,
                    matcher: matcher.map(str::to_string),
                    commands: pattern.map(str::to_string).into_iter().collect(),
                },
            )?;
        }
    }
    if out.is_empty() {
        bail!("give at least one event");
    }
    Ok(out)
}

/// A separator is an unescaped colon outside regex groups and classes.
/// Keep regex escapes intact for the agent. Everything after the separator
/// belongs to the command pattern, including any further colons.
fn split_matcher(text: &str) -> (&str, Option<&str>) {
    let mut escaped = false;
    let mut class = false;
    let mut depth = 0usize;
    for (offset, c) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if class {
            if c == ']' {
                class = false;
            }
            continue;
        }
        match c {
            '[' => class = true,
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => return (&text[..offset], Some(&text[offset + 1..])),
            _ => {}
        }
    }
    (text, None)
}

/// One matcher per event gives each call one Yappr handler. More tools can
/// share a matcher, such as Bash|Read, and command patterns combine with OR.
fn merge_selection(events: &mut Vec<Selected>, selected: Selected) -> Result<()> {
    if let Some(existing) = events.iter_mut().find(|s| s.event == selected.event) {
        if existing.matcher != selected.matcher {
            bail!(
                "use one matcher for {}. Combine alternatives with |, or repeat the same matcher",
                selected.event.key()
            );
        }
        if selected.commands.is_empty() {
            existing.commands.clear();
        } else if !existing.commands.is_empty() {
            for command in selected.commands {
                if !existing.commands.contains(&command) {
                    existing.commands.push(command);
                }
            }
        }
    } else {
        events.push(selected);
    }
    Ok(())
}

/// Literal whole-command matching, with only `*` and `?` as wildcards.
fn command_regex(pattern: &str) -> Result<regex::Regex> {
    if pattern.len() > 4096 {
        bail!("a command pattern must be at most 4096 bytes");
    }
    let escaped = regex::escape(pattern)
        .replace(r"\*", ".*")
        .replace(r"\?", ".");
    regex::Regex::new(&format!(r"(?s)\A{escaped}\z")).context("the command pattern is too large")
}

/// The fields of the event JSON that yappr uses. Both agents send them with
/// these names.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Input {
    pub hook_event_name: String,
    pub cwd: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_response: Option<Value>,
    pub error: Option<String>,
    pub message: Option<String>,
    pub last_assistant_message: Option<String>,
}

/// What yappr does for an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Ask for approval, and give the answer to the agent.
    Ask {
        event: Event,
        title: String,
        body: String,
        subject: Option<String>,
    },
    /// Post an entry. The agent gets no decision.
    Post {
        kind: String,
        title: String,
        body: String,
        subject: Option<String>,
    },
    /// Nothing to do for this event.
    Nothing,
}

/// The most bytes of agent text in one entry body.
const MAX_TEXT: usize = 8 * 1024;
/// The most bytes of a title.
const MAX_TITLE: usize = 200;

/// The action for one event of `agent`.
pub fn plan(agent: Agent, input: &Input) -> Action {
    let Some(event) = Event::from_key(&input.hook_event_name) else {
        return Action::Nothing;
    };
    if !event.supported(agent) {
        return Action::Nothing;
    }
    let subject = input.cwd.as_deref().map(subject_of);
    let name = agent.title();
    match event {
        Event::PermissionRequest | Event::PreToolUse => {
            let tool = input.tool_name.as_deref().unwrap_or("a tool");
            Action::Ask {
                event,
                title: one_line(&format!("{name}: {tool}"), MAX_TITLE),
                body: text(&describe(input.tool_input.as_ref())),
                subject,
            }
        }
        Event::PostToolUse | Event::PostToolUseFailure => {
            let tool = input.tool_name.as_deref().unwrap_or("a tool");
            let failed = event == Event::PostToolUseFailure;
            let result = match (&input.error, &input.tool_response) {
                (Some(error), _) => error.clone(),
                (_, Some(Value::String(result))) => result.clone(),
                (_, Some(result)) if !result.is_null() => {
                    serde_json::to_string_pretty(result).unwrap_or_default()
                }
                _ => "No result was provided.".into(),
            };
            Action::Post {
                kind: format!(
                    "{}.{}",
                    agent.name(),
                    if failed { "tool_failure" } else { "tool" }
                ),
                title: one_line(
                    &format!(
                        "{name}: {tool} {}",
                        if failed { "failed" } else { "completed" }
                    ),
                    MAX_TITLE,
                ),
                body: text(&format!(
                    "{}\n\nResult:\n{result}",
                    limited_text(&describe(input.tool_input.as_ref()), MAX_TEXT / 4)
                )),
                subject,
            }
        }
        Event::Notification => Action::Post {
            kind: format!("{}.notification", agent.name()),
            title: format!("{name} needs your input"),
            body: text(input.message.as_deref().unwrap_or_default()),
            subject,
        },
        Event::Stop | Event::SubagentStop => Action::Post {
            kind: format!(
                "{}.{}",
                agent.name(),
                if event == Event::Stop {
                    "stop"
                } else {
                    "subagent"
                }
            ),
            title: if event == Event::Stop {
                format!("{name} finished")
            } else {
                format!("{name} subagent finished")
            },
            body: text(input.last_assistant_message.as_deref().unwrap_or_default()),
            subject,
        },
        Event::SessionStart | Event::SessionEnd => Action::Post {
            kind: format!("{}.session", agent.name()),
            title: format!(
                "{name} session {}",
                if event == Event::SessionStart {
                    "started"
                } else {
                    "ended"
                }
            ),
            body: String::new(),
            subject,
        },
    }
}

/// Apply the command filters of the installed handler before it takes action.
/// The agent applies its tool-name matcher when selecting this handler.
pub fn plan_selected(agent: Agent, input: &Input, events: &[Selected]) -> Action {
    let command = input
        .tool_input
        .as_ref()
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str);
    let matches = events.iter().any(|selected| {
        selected.event.key() == input.hook_event_name
            && (selected.commands.is_empty()
                || command.is_some_and(|command| {
                    selected.commands.iter().any(|pattern| {
                        command_regex(pattern).is_ok_and(|regex| regex.is_match(command))
                    })
                }))
    });
    if matches {
        plan(agent, input)
    } else {
        Action::Nothing
    }
}

/// The command or the input of a tool, as people read it.
fn describe(input: Option<&Value>) -> String {
    match input {
        None => String::new(),
        Some(value) => match value.get("command").and_then(Value::as_str) {
            Some(command) => command.to_string(),
            None => serde_json::to_string_pretty(value).unwrap_or_default(),
        },
    }
}

/// A subject from a working directory, such as `project/yappr`. A subject
/// is one word.
fn subject_of(cwd: &str) -> String {
    let name = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or(cwd);
    let word: String = name
        .chars()
        .map(|c| {
            if c.is_whitespace() || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .take(200)
        .collect();
    format!("project/{}", if word.is_empty() { "-" } else { &word })
}

/// `text` with at most [`MAX_TEXT`] bytes and with no control characters
/// other than a newline and a tab, which an entry body does not allow.
fn text(text: &str) -> String {
    limited_text(text, MAX_TEXT)
}

fn limited_text(text: &str, max: usize) -> String {
    let mut out: String = text
        .chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            c if c.is_control() => '\u{fffd}',
            c => c,
        })
        .collect();
    if out.len() > max {
        // Cut on a character boundary, and keep room for the ellipsis.
        let mut end = max - '…'.len_utf8();
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push('…');
    }
    out
}

/// `text` on one line with at most `max` bytes.
fn one_line(text: &str, max: usize) -> String {
    let mut out = String::new();
    for c in text.chars().map(|c| if c.is_control() { ' ' } else { c }) {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

/// The JSON that tells the agent the answer to an [`Action::Ask`].
pub fn decision(agent: Agent, event: Event, approved: bool) -> Value {
    let reason = if approved {
        "Approved on a device with yappr"
    } else {
        "Rejected on a device with yappr"
    };
    match event {
        Event::PreToolUse => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": if approved { "allow" } else { "deny" },
                "permissionDecisionReason": reason,
            }
        }),
        _ => {
            let mut decision = Map::new();
            decision.insert(
                "behavior".into(),
                if approved { "allow" } else { "deny" }.into(),
            );
            // Codex needs a message with a denial. Claude Code shows it.
            if !approved || agent == Agent::Codex {
                decision.insert("message".into(), reason.into());
            }
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": decision,
                }
            })
        }
    }
}

/// The part of a hook command that marks it as an entry of yappr.
const MARK: &str = " hook --agent ";

/// The command that the agent runs for one event, with the feed directory
/// `dir` of the install.
pub fn command(
    program: &str,
    dir: &str,
    agent: Agent,
    device: Option<&str>,
    timeout_secs: u64,
) -> String {
    let mut out = format!(
        "{} --dir {}{MARK}{}",
        quote(program),
        quote(dir),
        agent.name()
    );
    if let Some(device) = device {
        out.push_str(&format!(" --to {}", quote(device)));
    }
    out.push_str(&format!(" --timeout {timeout_secs}s"));
    out
}

/// `text` in single quotes for a POSIX shell.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// True if this hook entry is one of yappr's.
fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains(MARK))
}

/// Add the hook entries for `events` to the settings `settings`, after the
/// entries of other tools, and replace earlier entries of yappr. A hook that
/// asks gets `ask_timeout` seconds plus a margin, so that its own deadline
/// comes first.
pub fn install(
    settings: &mut Value,
    events: &[Selected],
    command_for: impl Fn(Event) -> String,
    ask_timeout: u64,
) -> Result<()> {
    // Validate and merge before changing any existing settings. Callers may
    // construct selections directly rather than through parse_events.
    let mut selections = Vec::new();
    for selected in events {
        merge_selection(&mut selections, selected.clone())?;
    }
    remove(settings);
    let Some(root) = settings.as_object_mut() else {
        bail!("the settings file is not a JSON object");
    };
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("\"hooks\" in the settings file is not a JSON object");
    };
    for selected in &selections {
        let groups = hooks
            .entry(selected.event.key())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(groups) = groups.as_array_mut() else {
            bail!(
                "hooks.{} in the settings file is not a list",
                selected.event.key()
            );
        };
        let timeout = if selected.event.asks() {
            ask_timeout + 30
        } else {
            30
        };
        let mut group = Map::new();
        if let Some(matcher) = &selected.matcher {
            group.insert("matcher".into(), matcher.clone().into());
        }
        group.insert(
            "hooks".into(),
            json!([{
                "type": "command",
                "command": if selected.commands.is_empty() {
                    command_for(selected.event)
                } else {
                    format!("{} --events {}", command_for(selected.event), quote(&selected.selector()))
                },
                "timeout": timeout,
            }]),
        );
        groups.push(Value::Object(group));
    }
    Ok(())
}

/// Remove the hook entries of yappr, and the lists that become empty.
/// Returns true if there were any.
pub fn remove(settings: &mut Value) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut removed = false;
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = list.len();
                list.retain(|hook| !is_ours(hook));
                removed |= list.len() != before;
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|list| !list.is_empty())
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
    if hooks.is_empty() {
        if let Some(root) = settings.as_object_mut() {
            root.remove("hooks");
        }
    }
    removed
}

/// The installed events of yappr, with their commands.
pub fn installed(settings: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return out;
    };
    for (event, groups) in hooks {
        for group in groups.as_array().into_iter().flatten() {
            for hook in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if is_ours(hook) {
                    let command = hook
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    out.push((event.clone(), command.to_string()));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(json: Value) -> Input {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn a_permission_request_becomes_an_approval_with_the_command() {
        let action = plan(
            Agent::Claude,
            &input(json!({
                "hook_event_name": "PermissionRequest",
                "cwd": "/Users/me/my project",
                "tool_name": "Bash",
                "tool_input": {"command": "rm -rf build/\u{1b}[2J", "description": null},
            })),
        );
        let Action::Ask {
            event,
            title,
            body,
            subject,
        } = action
        else {
            panic!("{action:?}")
        };
        assert_eq!(event, Event::PermissionRequest);
        assert_eq!(title, "Claude: Bash");
        assert_eq!(body, "rm -rf build/\u{fffd}[2J");
        assert_eq!(subject.as_deref(), Some("project/my-project"));
    }

    #[test]
    fn other_events_become_posts_and_unknown_events_do_nothing() {
        let stop = plan(
            Agent::Codex,
            &input(json!({"hook_event_name": "Stop", "last_assistant_message": "Done."})),
        );
        assert!(
            matches!(stop, Action::Post { ref kind, ref body, .. } if kind == "codex.stop" && body == "Done.")
        );
        let long = "x".repeat(MAX_TEXT * 2);
        let Action::Post { body, .. } = plan(
            Agent::Claude,
            &input(json!({"hook_event_name": "Notification", "message": long})),
        ) else {
            panic!()
        };
        assert!(body.len() <= MAX_TEXT && body.ends_with('…'));
        // A body at the limit stays whole. One byte more is cut, with the
        // ellipsis inside the limit, also in the middle of a character.
        assert_eq!(text(&"x".repeat(MAX_TEXT)), "x".repeat(MAX_TEXT));
        assert_eq!(text(&"x".repeat(MAX_TEXT + 1)).len(), MAX_TEXT);
        let wide = text(&"é".repeat(MAX_TEXT));
        assert!(wide.len() <= MAX_TEXT && wide.ends_with('…'));
        assert_eq!(
            plan(
                Agent::Claude,
                &input(json!({"hook_event_name": "UnknownEvent"}))
            ),
            Action::Nothing
        );
    }

    #[test]
    fn decisions_have_the_shape_of_each_event_and_agent() {
        assert_eq!(
            decision(Agent::Claude, Event::PermissionRequest, true),
            json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}})
        );
        let deny = decision(Agent::Codex, Event::PermissionRequest, false);
        assert_eq!(deny["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert!(deny["hookSpecificOutput"]["decision"]["message"].is_string());
        let pre = decision(Agent::Claude, Event::PreToolUse, false);
        assert_eq!(pre["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    #[test]
    fn install_keeps_other_hooks_and_replaces_its_own() {
        let mut settings = json!({
            "model": "x",
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other-tool"}]}]},
        });
        let command = |_: Event| {
            command(
                "/example/yappr",
                "/example/feed",
                Agent::Claude,
                Some("phone"),
                120,
            )
        };
        install(&mut settings, &default_events(Agent::Claude), command, 120).unwrap();
        install(&mut settings, &default_events(Agent::Claude), command, 120).unwrap();
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "{settings}");
        assert_eq!(stop[0]["hooks"][0]["command"], "other-tool");
        assert_eq!(
            settings["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"],
            150
        );
        assert_eq!(installed(&settings).len(), 2);
        assert_eq!(settings["model"], "x");

        assert!(remove(&mut settings));
        assert_eq!(
            settings,
            json!({"model": "x", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other-tool"}]}]}})
        );
        let mut empty = json!({});
        install(&mut empty, &default_events(Agent::Codex), command, 120).unwrap();
        assert!(empty["hooks"].get("Notification").is_none());
        remove(&mut empty);
        assert_eq!(empty, json!({}));
    }

    #[test]
    fn events_parse_with_tool_matchers() {
        let events = parse_events(Agent::Claude, "permission, pretooluse:Bash, session").unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[1].matcher.as_deref(), Some("Bash"));
        assert!(parse_events(Agent::Codex, "notification").is_err());
        assert!(parse_events(Agent::Claude, "pretooluse").is_err());
        assert!(parse_events(Agent::Claude, "loud").is_err());
    }

    #[test]
    fn command_filters_belong_to_each_event_and_merge_repeated_patterns() {
        let events = parse_events(
            Agent::Claude,
            "permission,stop,pretooluse:Bash:git push*,posttooluse:Bash:cargo test*,posttooluse:Bash:cargo build*,posttooluse:Bash:cargo test*",
        ).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[2].commands, ["git push*"]);
        assert_eq!(events[3].commands, ["cargo test*", "cargo build*"]);
        for (event, command, matched) in [
            ("PreToolUse", "git push origin main", true),
            ("PreToolUse", "cargo test", false),
            ("PostToolUse", "cargo test --release", true),
            ("PostToolUse", "cargo build", true),
            ("PostToolUse", "git push origin main", false),
            ("PermissionRequest", "rm -rf build", true),
            ("Stop", "", true),
        ] {
            let action = plan_selected(
                Agent::Claude,
                &input(json!({
                    "hook_event_name": event,
                    "tool_name": "Bash",
                    "tool_input": {"command": command},
                    "tool_response": "done",
                })),
                &events,
            );
            assert_eq!(action != Action::Nothing, matched, "{event}: {command}");
        }
    }

    #[test]
    fn command_patterns_are_literal_globs_over_the_whole_command() {
        for (pattern, command, matched) in [
            ("cargo test*", "cargo test", true),
            ("cargo test*", "cd app && cargo test", false),
            ("*cargo test*", "cd app && cargo test", true),
            ("cargo test", "cargo test --release", false),
            ("cargo ?est", "cargo test", true),
            ("echo ?", "echo é", true),
            ("echo ?", "echo éé", false),
            ("echo [ok].+$", "echo [ok].+$", true),
            ("echo [ok].+$", "echo oooo", false),
            ("echo *", "echo one\ntwo", true),
        ] {
            assert_eq!(
                command_regex(pattern).unwrap().is_match(command),
                matched,
                "{pattern}: {command}"
            );
        }
        let events = parse_events(Agent::Claude, "pretooluse:Bash:*").unwrap();
        assert_eq!(
            plan_selected(
                Agent::Claude,
                &input(json!({
                    "hook_event_name": "PreToolUse", "tool_input": {}
                })),
                &events
            ),
            Action::Nothing
        );
    }

    #[test]
    fn invalid_filters_are_rejected_and_colons_stay_in_the_pattern() {
        for value in [
            "permission:Bash:git *",
            "stop:Bash:*",
            "session::*",
            "pretooluse::git *",
            "posttooluse:Bash:",
        ] {
            assert!(parse_events(Agent::Claude, value).is_err(), "{value}");
        }
        assert!(parse_events(Agent::Codex, "posttoolusefailure:Bash:*").is_err());
        let selected =
            parse_events(Agent::Claude, "posttooluse:Bash:curl https://example.com/*").unwrap();
        assert_eq!(selected[0].commands, ["curl https://example.com/*"]);
        assert_eq!(
            parse_events(Agent::Claude, &selected[0].selector()).unwrap(),
            selected
        );
        for value in [
            "posttooluse:Bash,posttooluse:Bash:cargo *",
            "posttooluse:Bash:cargo *,posttooluse:Bash",
        ] {
            assert!(parse_events(Agent::Claude, value).unwrap()[0]
                .commands
                .is_empty());
        }
    }

    #[test]
    fn regex_matchers_keep_colons_and_round_trip_with_command_filters() {
        for (selection, matcher, commands) in [
            ("pretooluse:(?:Bash|Write)", "(?:Bash|Write)", vec![]),
            ("permission:(?:Bash|Write)", "(?:Bash|Write)", vec![]),
            ("pretooluse:(?i:Bash)", "(?i:Bash)", vec![]),
            ("pretooluse:[B:ash]+", "[B:ash]+", vec![]),
            (
                r"pretooluse:mcp__server\:tool",
                r"mcp__server\:tool",
                vec![],
            ),
            (
                "pretooluse:(?:Bash|Write):git push*",
                "(?:Bash|Write)",
                vec!["git push*"],
            ),
            (
                r"posttooluse:mcp__server\:tool:curl https://host/*",
                r"mcp__server\:tool",
                vec!["curl https://host/*"],
            ),
            (
                r"posttooluse:Bash\\:cargo test*",
                r"Bash\\",
                vec!["cargo test*"],
            ),
        ] {
            let events = parse_events(Agent::Claude, selection).unwrap();
            assert_eq!(events[0].matcher.as_deref(), Some(matcher), "{selection}");
            assert_eq!(events[0].commands, commands, "{selection}");
            assert_eq!(
                parse_events(Agent::Claude, &events[0].selector()).unwrap(),
                events
            );
        }
    }

    #[test]
    fn conflicting_event_matchers_cannot_install_duplicate_actions() {
        for value in [
            "posttooluse:Bash:cargo test*,posttooluse:Bash|Read:*test*",
            "pretooluse:Bash:git push*,pretooluse:Bash|Read:git *",
            "posttooluse,posttooluse:Bash:cargo test*",
        ] {
            let error = parse_events(Agent::Claude, value).unwrap_err().to_string();
            assert!(error.contains("one matcher"), "{error}");
        }
        // Direct callers of install must get the same check, before existing
        // settings or hooks are changed.
        let mut events = parse_events(Agent::Claude, "posttooluse:Bash:cargo test*").unwrap();
        events.extend(parse_events(Agent::Claude, "posttooluse:Bash|Read:*test*").unwrap());
        let mut settings =
            json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other-tool"}]}]}});
        let original = settings.clone();
        assert!(install(&mut settings, &events, |_| String::new(), 120).is_err());
        assert_eq!(settings, original);

        let combined = parse_events(
            Agent::Claude,
            "posttooluse:Bash|Read:cargo test*,posttooluse:Bash|Read:*test*",
        )
        .unwrap();
        install(
            &mut settings,
            &combined,
            |_| command("yappr", "/feed", Agent::Claude, None, 120),
            120,
        )
        .unwrap();
        assert_eq!(
            settings["hooks"]["PostToolUse"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn completed_tools_include_the_command_and_result_without_a_decision() {
        for (agent, event, response, error, kind, title) in [
            (
                Agent::Claude,
                "PostToolUse",
                json!({"stdout": "3 passed"}),
                None,
                "claude.tool",
                "Claude: Bash completed",
            ),
            (
                Agent::Codex,
                "PostToolUse",
                json!("Exit code: 1\n1 failed"),
                None,
                "codex.tool",
                "Codex: Bash completed",
            ),
            (
                Agent::Claude,
                "PostToolUseFailure",
                Value::Null,
                Some("Exit code 1: failed"),
                "claude.tool_failure",
                "Claude: Bash failed",
            ),
        ] {
            let action = plan(
                agent,
                &input(json!({
                    "hook_event_name": event, "cwd": "/work/app", "tool_name": "Bash",
                    "tool_input": {"command": "cargo test"}, "tool_response": response, "error": error,
                })),
            );
            let Action::Post {
                kind: actual_kind,
                title: actual_title,
                body,
                subject,
            } = action
            else {
                panic!("{action:?}")
            };
            assert_eq!(actual_kind, kind);
            assert_eq!(actual_title, title);
            assert!(body.contains("cargo test"));
            assert!(body.contains(if event == "PostToolUseFailure" {
                "Exit code 1: failed"
            } else if agent == Agent::Codex {
                "1 failed"
            } else {
                "3 passed"
            }));
            assert_eq!(subject.as_deref(), Some("project/app"));
        }
        let Action::Post { body, .. } = plan(
            Agent::Claude,
            &input(json!({
                "hook_event_name": "PostToolUse", "tool_response": "\u{1b}[31m".to_owned() + &"é".repeat(MAX_TEXT)
            })),
        ) else {
            panic!()
        };
        assert!(body.len() <= MAX_TEXT && body.ends_with('…'));
        assert!(!body.contains('\u{1b}'));
        let Action::Post { body, .. } = plan(
            Agent::Claude,
            &input(json!({
                "hook_event_name": "PostToolUse", "tool_input": {"content": "x".repeat(MAX_TEXT * 2)},
                "tool_response": "The file was written.",
            })),
        ) else {
            panic!()
        };
        assert!(body.contains("The file was written."));
        assert!(body.len() <= MAX_TEXT);
    }

    #[test]
    fn install_preserves_filters_in_one_handler_per_event_and_matcher() {
        let events = parse_events(
            Agent::Claude,
            "permission,posttooluse:Bash:cargo test*,posttooluse:Bash:cargo build*",
        )
        .unwrap();
        let mut settings = json!({});
        install(
            &mut settings,
            &events,
            |_| command("yappr", "/feed", Agent::Claude, None, 120),
            120,
        )
        .unwrap();
        let groups = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["matcher"], "Bash");
        assert_eq!(groups[0]["hooks"][0]["timeout"], 30);
        let command = groups[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(
            command
                .contains("--events 'PostToolUse:Bash:cargo test*,PostToolUse:Bash:cargo build*'"),
            "{command}"
        );
        assert!(
            !settings["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("--events")
        );
        assert!(remove(&mut settings));
        assert_eq!(settings, json!({}));
    }

    #[test]
    fn a_command_quotes_the_program_and_the_device() {
        assert_eq!(
            command("/example path/yappr", "/example/feed", Agent::Codex, Some("it's"), 150),
            "'/example path/yappr' --dir '/example/feed' hook --agent codex --to 'it'\\''s' --timeout 150s"
        );
    }
}
