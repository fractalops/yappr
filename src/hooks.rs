//! Hooks for coding agents: Claude Code and Codex run `yappr hook` for an
//! event, such as a permission request, and yappr asks a device or posts an
//! entry.
//!
//! The agent plugins in this repository run `yappr hook` for every event
//! that yappr supports. [`parse_events`] reads the events that the user
//! selected, and [`plan_selected`] turns the event JSON that the agent sends
//! on stdin into an [`Action`]. [`decision`] is the JSON that the agent reads
//! back.
use std::{fmt, time::Duration};

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

/// The timeout of an approval hook in the plugin hook files, in seconds.
pub const ASK_HOOK_TIMEOUT: u64 = 3600;
/// The longest wait for an answer. The hook gives its own result before the
/// agent stops it at [`ASK_HOOK_TIMEOUT`].
pub const MAX_WAIT: Duration = Duration::from_secs(ASK_HOOK_TIMEOUT - 60);
/// The wait for an answer when the settings give none.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(120);

const ALL: [Event; 7] = [
    Event::PermissionRequest,
    Event::PreToolUse,
    Event::PostToolUse,
    Event::PostToolUseFailure,
    Event::Notification,
    Event::Stop,
    Event::SubagentStop,
];

/// The events when the settings select none.
pub fn default_events() -> Vec<Selected> {
    // A Claude notification repeats a permission request or a stop, so it is
    // not a default.
    [Event::PermissionRequest, Event::Stop]
        .into_iter()
        .map(|event| Selected {
            event,
            matcher: None,
            commands: Vec::new(),
        })
        .collect()
}

/// A selected event, with its tool matcher and command patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub event: Event,
    /// A regular expression for the whole tool name, such as `Bash|Read`.
    /// None accepts every tool.
    pub matcher: Option<String>,
    /// Any pattern may match. An empty list accepts every command.
    pub commands: Vec<String>,
}

/// Parse `event[:matcher[:command-pattern]]` entries separated by commas.
/// An event that an agent does not have never occurs for that agent.
pub fn parse_events(text: &str) -> Result<Vec<Selected>> {
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
            "subagent" | "subagentstop" => vec![Event::SubagentStop],
            other => bail!(
                "unknown event {other:?}. Use permission, pretooluse:TOOL, posttooluse, posttoolusefailure, notification, stop, or subagent"
            ),
        };
        if let Some(matcher) = matcher {
            matcher_regex(matcher)?;
        }
        for event in events {
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

/// The tool matcher as a regular expression over the whole tool name.
fn matcher_regex(matcher: &str) -> Result<regex::Regex> {
    regex::Regex::new(&format!(r"\A(?:{matcher})\z"))
        .with_context(|| format!("the tool matcher {matcher:?} is not a valid regular expression"))
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
        subject: String,
    },
    /// Update the activity card of the agent for this project: one `state`
    /// entry per subject, which replaces the earlier one in the app.
    Activity {
        title: String,
        /// One line for the list of recent steps. None starts a new list.
        step: Option<String>,
        /// Markdown below the steps, such as a result or the last message.
        detail: String,
        subject: String,
    },
    /// Post an entry of its own. The agent gets no decision.
    Post {
        kind: String,
        title: String,
        body: String,
        subject: String,
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
    let subject = subject_of(agent, input.cwd.as_deref());
    let name = agent.title();
    let tool = input.tool_name.as_deref().unwrap_or("a tool");
    match event {
        Event::PermissionRequest | Event::PreToolUse => Action::Ask {
            event,
            title: one_line(&format!("{name} asks to use {tool}"), MAX_TITLE),
            body: text(&describe(input.tool_input.as_ref())),
            subject,
        },
        Event::PostToolUse | Event::PostToolUseFailure => {
            let failed = event == Event::PostToolUseFailure;
            let result = match &input.error {
                Some(error) => error.clone(),
                None => input
                    .tool_response
                    .as_ref()
                    .map(output_of)
                    .unwrap_or_default(),
            };
            let target = summary(input.tool_input.as_ref(), input.cwd.as_deref());
            let mut step = format!("{}{}", code(tool), if failed { " failed" } else { "" });
            if !target.is_empty() {
                step.push_str(&format!(" · {}", code(&target)));
            }
            let detail = if result.trim().is_empty() {
                String::new()
            } else {
                format!(
                    "{}\n{}",
                    if failed { "Error:" } else { "Last result:" },
                    fenced(&limited_text(result.trim(), MAX_RESULT))
                )
            };
            Action::Activity {
                title: format!("{name} is working"),
                step: Some(step),
                detail,
                subject,
            }
        }
        Event::Notification => Action::Post {
            kind: format!("{}.notification", agent.name()),
            title: format!("{name} needs your input"),
            body: text(input.message.as_deref().unwrap_or_default()),
            subject,
        },
        Event::Stop | Event::SubagentStop => Action::Activity {
            title: if event == Event::Stop {
                format!("{name} finished")
            } else {
                format!("{name}: a subagent finished")
            },
            step: None,
            detail: text(input.last_assistant_message.as_deref().unwrap_or_default()),
            subject,
        },
    }
}

/// The action for one event of `agent`, if the event, its tool and its
/// command match one of the selected `events`.
pub fn plan_selected(agent: Agent, input: &Input, events: &[Selected]) -> Action {
    let command = input
        .tool_input
        .as_ref()
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str);
    let tool = input.tool_name.as_deref();
    let matches = events.iter().any(|selected| {
        selected.event.key() == input.hook_event_name
            && selected.matcher.as_deref().is_none_or(|matcher| {
                tool.is_some_and(|tool| matcher_regex(matcher).is_ok_and(|r| r.is_match(tool)))
            })
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

/// The most input text in one recent step.
const MAX_STEP: usize = 80;
/// The most bytes of a tool result on the activity card.
const MAX_RESULT: usize = 2 * 1024;

/// The text output of a tool: a text result, or the stdout and stderr of a
/// command. Other results, such as the content of a file that the tool
/// read, are not shown.
fn output_of(response: &Value) -> String {
    match response {
        Value::String(text) => text.clone(),
        Value::Object(fields) => ["stdout", "stderr", "output"]
            .iter()
            .filter_map(|key| fields.get(*key).and_then(Value::as_str))
            .map(str::trim_end)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The main argument of a tool, on one line, such as the command or the
/// file path. A path in the working directory `cwd` is relative.
fn summary(input: Option<&Value>, cwd: Option<&str>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let main = [
        "command",
        "file_path",
        "path",
        "url",
        "pattern",
        "query",
        "description",
    ]
    .iter()
    .find_map(|key| input.get(key).and_then(Value::as_str))
    .map(str::to_owned)
    .or_else(|| {
        input
            .as_object()
            .and_then(|o| o.values().find_map(Value::as_str))
            .map(str::to_owned)
    })
    .unwrap_or_default();
    let main = match cwd.map(|c| format!("{}/", c.trim_end_matches('/'))) {
        Some(prefix) => main
            .strip_prefix(&prefix)
            .map(str::to_owned)
            .unwrap_or(main),
        None => main,
    };
    let line = main.lines().next().unwrap_or_default();
    let mut out = one_line(line, MAX_STEP);
    if out.len() < main.trim_end().len() {
        out.push('…');
    }
    out
}

/// `text` as a Markdown code span.
fn code(text: &str) -> String {
    let ticks = "`".repeat(longest_run(text, '`') + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{ticks}{pad}{text}{pad}{ticks}")
}

/// `text` as a fenced Markdown code block.
fn fenced(text: &str) -> String {
    let fence = "`".repeat((longest_run(text, '`') + 1).max(3));
    format!("{fence}\n{text}\n{fence}")
}

fn longest_run(text: &str, c: char) -> usize {
    text.split(|x| x != c).map(str::len).max().unwrap_or(0)
}

/// The subject of an agent in a working directory, such as `claude/yappr`.
/// A subject is one word.
fn subject_of(agent: Agent, cwd: Option<&str>) -> String {
    let Some(cwd) = cwd else {
        return agent.name().to_owned();
    };
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
    format!(
        "{}/{}",
        agent.name(),
        if word.is_empty() { "-" } else { &word }
    )
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
        assert_eq!(title, "Claude asks to use Bash");
        assert_eq!(body, "rm -rf build/\u{fffd}[2J");
        assert_eq!(subject, "claude/my-project");
    }

    #[test]
    fn stops_update_the_activity_card_and_unknown_events_do_nothing() {
        let stop = plan(
            Agent::Codex,
            &input(json!({"hook_event_name": "Stop", "last_assistant_message": "Done."})),
        );
        assert_eq!(
            stop,
            Action::Activity {
                title: "Codex finished".into(),
                step: None,
                detail: "Done.".into(),
                subject: "codex".into(),
            }
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
    fn events_parse_with_tool_matchers() {
        let events = parse_events("permission, pretooluse:Bash, subagent").unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].matcher.as_deref(), Some("Bash"));
        assert!(parse_events("pretooluse").is_err());
        assert!(parse_events("loud").is_err());
        assert!(parse_events("session").is_err());
    }

    #[test]
    fn command_filters_belong_to_each_event_and_merge_repeated_patterns() {
        let events = parse_events("permission,stop,pretooluse:Bash:git push*,posttooluse:Bash:cargo test*,posttooluse:Bash:cargo build*,posttooluse:Bash:cargo test*",
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
        let events = parse_events("pretooluse:Bash:*").unwrap();
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
            assert!(parse_events(value).is_err(), "{value}");
        }
        let selected = parse_events("posttooluse:Bash:curl https://example.com/*").unwrap();
        assert_eq!(selected[0].commands, ["curl https://example.com/*"]);
        for value in [
            "posttooluse:Bash,posttooluse:Bash:cargo *",
            "posttooluse:Bash:cargo *,posttooluse:Bash",
        ] {
            assert!(parse_events(value).unwrap()[0].commands.is_empty());
        }
    }

    #[test]
    fn regex_matchers_keep_colons_before_command_filters() {
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
            let events = parse_events(selection).unwrap();
            assert_eq!(events[0].matcher.as_deref(), Some(matcher), "{selection}");
            assert_eq!(events[0].commands, commands, "{selection}");
        }
    }

    #[test]
    fn conflicting_event_matchers_are_rejected() {
        for value in [
            "posttooluse:Bash:cargo test*,posttooluse:Bash|Read:*test*",
            "pretooluse:Bash:git push*,pretooluse:Bash|Read:git *",
            "posttooluse,posttooluse:Bash:cargo test*",
        ] {
            let error = parse_events(value).unwrap_err().to_string();
            assert!(error.contains("one matcher"), "{error}");
        }
        let combined =
            parse_events("posttooluse:Bash|Read:cargo test*,posttooluse:Bash|Read:*test*").unwrap();
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].commands, ["cargo test*", "*test*"]);
    }

    #[test]
    fn completed_tools_add_a_step_and_the_result_to_the_activity_card() {
        for (agent, event, response, error, step, result) in [
            (
                Agent::Claude,
                "PostToolUse",
                json!({"stdout": "3 passed"}),
                None,
                "`Bash` · `cargo test`",
                "Last result:",
            ),
            (
                Agent::Codex,
                "PostToolUse",
                json!("Exit code: 1\n1 failed"),
                None,
                "`Bash` · `cargo test`",
                "1 failed",
            ),
            (
                Agent::Claude,
                "PostToolUseFailure",
                Value::Null,
                Some("Exit code 1: failed"),
                "`Bash` failed · `cargo test`",
                "Error:",
            ),
        ] {
            let action = plan(
                agent,
                &input(json!({
                    "hook_event_name": event, "cwd": "/work/app", "tool_name": "Bash",
                    "tool_input": {"command": "cargo test"}, "tool_response": response, "error": error,
                })),
            );
            let Action::Activity {
                title,
                step: Some(actual_step),
                detail,
                subject,
            } = action
            else {
                panic!("{action:?}")
            };
            assert_eq!(title, format!("{} is working", agent.title()));
            assert_eq!(actual_step, step);
            assert!(detail.contains(result), "{detail}");
            assert_eq!(subject, format!("{agent}/app"));
        }
        let Action::Activity { detail, .. } = plan(
            Agent::Claude,
            &input(json!({
                "hook_event_name": "PostToolUse", "tool_response": "\u{1b}[31m".to_owned() + &"é".repeat(MAX_TEXT)
            })),
        ) else {
            panic!()
        };
        assert!(detail.len() <= MAX_RESULT + 32 && detail.contains('…'));
        assert!(!detail.contains('\u{1b}'));
    }

    #[test]
    fn steps_name_the_main_argument_on_one_line_as_code() {
        for (input, step) in [
            (
                json!({"file_path": "/w/src/main.rs", "content": "x"}),
                "`Write` · `/w/src/main.rs`",
            ),
            (
                json!({"command": "echo `date`\nls"}),
                "`Write` · ``echo `date`…``",
            ),
            (json!({}), "`Write`"),
        ] {
            let Action::Activity {
                step: Some(actual), ..
            } = plan(
                Agent::Claude,
                &self::input(
                    json!({"hook_event_name": "PostToolUse", "tool_name": "Write", "tool_input": input}),
                ),
            )
            else {
                panic!()
            };
            assert_eq!(actual, step);
        }
        assert_eq!(fenced("a ``` b"), "````\na ``` b\n````");
        assert_eq!(
            output_of(&json!({"stdout": "ok\n", "stderr": "", "interrupted": false})),
            "ok"
        );
        assert_eq!(
            output_of(&json!({"type": "text", "file": {"content": "x"}})),
            ""
        );
    }

    #[test]
    fn matchers_select_whole_tool_names() {
        let events = parse_events("permission:Bash|Write,posttooluse:mcp__.*,stop").unwrap();
        for (event, tool, matched) in [
            ("PermissionRequest", Some("Bash"), true),
            ("PermissionRequest", Some("Write"), true),
            ("PermissionRequest", Some("BashOutput"), false),
            ("PermissionRequest", None, false),
            ("PostToolUse", Some("mcp__github__search"), true),
            ("PostToolUse", Some("Bash"), false),
            ("Stop", None, true),
            ("PreToolUse", Some("Bash"), false),
        ] {
            let action = plan_selected(
                Agent::Claude,
                &input(json!({"hook_event_name": event, "tool_name": tool, "tool_input": {}})),
                &events,
            );
            assert_eq!(action != Action::Nothing, matched, "{event}: {tool:?}");
        }
        assert!(parse_events("permission:(Bash").is_err());
    }

    #[test]
    fn events_that_an_agent_lacks_do_nothing_for_it() {
        let events = parse_events("notification,posttoolusefailure").unwrap();
        for event in ["Notification", "PostToolUseFailure"] {
            let input = input(json!({"hook_event_name": event, "message": "x", "error": "x"}));
            assert_eq!(
                plan_selected(Agent::Codex, &input, &events),
                Action::Nothing
            );
            assert_ne!(
                plan_selected(Agent::Claude, &input, &events),
                Action::Nothing
            );
        }
    }
}
