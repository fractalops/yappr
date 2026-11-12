# Agent hooks (experimental)

> [!WARNING]
> Agent hooks are experimental. Their behavior and settings can change in any release.

The yappr-hooks plugin connects Claude Code and Codex to your other device, such as your phone.
When you are away from the computer, you approve or reject the agent's permission requests on your other device. The device also shows when the agent finishes.
When you are at the computer, the plugin does nothing. The agent works as it does without the plugin.

yappr-hooks is separate from the yappr plugin. The yappr plugin has no hooks. It lets the agent send updates and ask questions when the agent decides to.
You can install one plugin, or both.

## Install

Install Yappr and pair your other device first. See the [README](../README.md).
For Claude Code:

```sh
claude plugin marketplace add fractalops/yappr
claude plugin install yappr-hooks@yappr
```

For Codex:

```sh
codex plugin marketplace add fractalops/yappr
codex plugin add yappr-hooks@yappr
```

Codex runs new hooks only after you trust them. Start Codex and type `/hooks`. Trust the Yappr hooks.
Do this again after each plugin update.

If you installed hooks with Yappr 0.1.8, remove them before you install yappr-hooks.
Delete the hook entries whose command contains `hook --agent` from the settings file where you installed them.
Keep the other hooks in that file.

| Scope | Claude Code | Codex |
|---|---|---|
| User | `~/.claude/settings.json` | `~/.codex/hooks.json` |
| Project | `.claude/settings.json` | `.codex/hooks.json` |
| Local | `.claude/settings.local.json` | |

If you set `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, look in that directory for the user file.
If you keep the old hooks, each request can go to your other device twice.

To remove the hooks, remove the plugin: `claude plugin uninstall yappr-hooks@yappr` or `codex plugin remove yappr-hooks@yappr`.

## How it works

An agent calls a hook at fixed points: before a tool runs, when it needs permission, after a tool runs, and when it finishes.
The plugin runs `yappr hook` at each of these points. Yappr then asks three questions:

1. **Is this event selected?** By default, Yappr selects two events: a permission request, and the end of a turn.
   For other events, Yappr stops at once and the agent continues.
2. **Are you away?** You are away when the screen is locked, or after 2 minutes with no keyboard or mouse input.
   If you are at the computer, Yappr stops and the agent continues as usual.
   If Yappr cannot tell, it acts as if you are at the computer.
3. **What does the event need?**
   - The agent finished: Yappr shows the last message of the agent on your other device. The agent does not wait.
   - The agent needs permission: Yappr sends the request to your other device and waits for your answer.

A permission request has four possible results:

| What happens | Result |
|---|---|
| You tap **Approve** | The tool runs. |
| You tap **Reject** | The agent gets a refusal. |
| No answer in 2 minutes | The agent shows its usual terminal prompt. |
| You come back to the computer | Yappr cancels the request on your other device. The agent shows its usual terminal prompt. |

Yappr checks every 3 seconds whether you came back.

## What you see on your other device

- **A request**, such as "Claude asks to use Bash in yappr", with the command. It has **Approve** and **Reject** buttons.
- **A card for each agent and project**, such as "Claude finished in yappr", with the last message of the agent.
  The app shows only the newest card for each agent and project.

You can also select tool steps for the card. See [Events](#events).

## When something fails

If Yappr is missing or reports an error, the agent continues with its usual permission rules.
If your device is unreachable, a request can wait until its timeout.

- If Yappr is not installed, the plugin writes an install hint to stderr. Codex shows the hint as a warning.
- If your other device is not reachable, the entry stays in the feed. `yappr start` or `yappr retry` delivers it later.
- Errors go to stderr. In Claude Code, start the agent with `--debug-file FILE` to see them.

## Change what the plugin does

Use `yappr config`:

```sh
# Ask the device named phone, and wait 5 minutes for an answer
yappr config --hook-to phone --hook-timeout 5m

# Also act when you are at the computer
yappr config --hook-when always

# Restore the defaults
yappr config --hook-to "" --hook-timeout "" --hook-events "" --hook-when ""
```

| Setting | What it does | Default |
|---|---|---|
| `--hook-to DEVICE` | The device that answers | The one device that receives this feed |
| `--hook-timeout DURATION` | How long a request waits. Maximum: `59m` | `2m` |
| `--hook-events LIST` | The selected events. See [Events](#events) | `permission,stop` |
| `--hook-when away\|always` | When the hooks act | `away` |

If several devices can answer, set `--hook-to`.

Claude Code has plugin options with the same settings: **Device**, **Approval wait**, **Events**, **When**, and **Feed**.
You can leave all five options empty.
Use `/plugin configure yappr-hooks@yappr` to set them, then restart Claude Code.
A plugin option replaces the `yappr config` setting. An empty option keeps the `yappr config` setting.

Codex has no plugin options. Use `yappr config`, or set these environment variables before you start Codex:
`YAPPR_HOOK_TO`, `YAPPR_HOOK_TIMEOUT`, `YAPPR_HOOK_EVENTS`, `YAPPR_HOOK_WHEN`, and `YAPPR_FEED`.
The environment variables also work in Claude Code.

## Events

`--hook-events` replaces the default list. To keep the defaults, include `permission,stop`.

| Event | Agent event | What Yappr does |
|---|---|---|
| `permission` | `PermissionRequest` | Asks for approval and gives your answer to the agent |
| `stop` | `Stop` | Shows the last message of the agent on its card |
| `subagent` | `SubagentStop` | Shows the last message of a subagent on the card |
| `pretooluse:TOOL` | `PreToolUse` | Asks for approval before each matching tool runs |
| `posttooluse[:TOOL]` | `PostToolUse` | Adds a step to the card, with the command output |
| `posttoolusefailure[:TOOL]` | `PostToolUseFailure`, Claude Code only | Adds a failed step to the card, with the error |
| `notification` | `Notification`, Claude Code only | Sends the notification of the agent as a separate entry |

Example: also ask before `git push`, and show test results on the card.

```sh
yappr config --hook-events 'permission,stop,pretooluse:Bash:git push*,posttooluse:Bash:cargo test*'
```

With tool steps selected, the card shows the last 5 steps. Each step shows the tool and its main argument, such as a command or a file path.
Below the steps, the card shows the stdout and stderr of the last command, up to 2 KiB.
The card does not show the content of files that the agent reads or edits. If a command prints a file, the output can include that file.
After the agent finishes, the next step starts a new list.

`pretooluse` asks for each matching tool. Select only the tools that you want to approve, such as `pretooluse:Bash:git push*`.

### Command filters

Each event entry has the form `event[:matcher[:command-pattern]]`.

- The **matcher** is a regular expression for the full tool name, such as `Bash` or `Edit|Write`.
- The **command pattern** compares with the command of the tool. It works for `pretooluse`, `posttooluse`, and `posttoolusefailure`.
  `permission` does not accept a command pattern.

Patterns compare with the full command and are case-sensitive. `*` matches any text, also no text. `?` matches one character.
All other characters match only themselves. Yappr does not read shell syntax.

| Pattern | Matches | Does not match |
|---|---|---|
| `cargo test` | `cargo test` | `cargo test --release` |
| `cargo test*` | `cargo test`, `cargo test --release` | `cd app && cargo test` |
| `*cargo test*` | `cd app && cargo test` | `cargo build` |

A pattern selects commands by their text. It does not find every command that a shell can run.
For example, `*cargo test*` also matches `echo "cargo test"`.

Rules for the list:

- Quote the list to protect spaces, `*`, and `|` from the shell.
- Use one matcher for each event. Combine tools with `|`, such as `Bash|Read`.
- Repeat an event with the same matcher to accept more than one command pattern.
- An entry without a command pattern accepts all calls of its tool.
- Commas separate entries. A matcher or a pattern cannot contain a comma.
- In a matcher, write a colon as `\:`, such as `posttooluse:mcp__server\:tool`. Colons in regex groups and classes, such as `(?:Bash|Write)`, need no escape.
- The first colon after the matcher starts the command pattern. Other colons are part of the pattern.
- A command pattern can have a maximum of 4096 bytes.

Claude Code reports a failed tool with `PostToolUseFailure`. Codex reports a failed command with `PostToolUse`.

## Details

**How Yappr finds out if you are away**

- macOS: Yappr reads the lock state and the idle time from `ioreg`.
- Linux: Yappr reads `LockedHint` and `IdleHint` of the graphical session from `loginctl`. The desktop sets `IdleHint`, and its idle time can be longer than 2 minutes.
- Over SSH, in a container, and on Windows, Yappr cannot tell. The hooks act only with `--hook-when always`.

**How the plugin finds Yappr**

The plugin looks for `yappr` on `PATH`, then in `~/.cargo/bin`. Set `YAPPR_BIN` to use a different binary.
To test a binary in the Codex CLI, start a local session: `YAPPR_BIN=/path/to/yappr codex --no-daemon`.
A shared Codex server can use a different environment.

**Limits**

- A hook reads a maximum of 1 MiB of event input. Yappr cuts entry bodies at 8 KiB.
- Each hook gives an exit code of 0. An error never blocks the agent.
- Claude Code can show its terminal prompt while Yappr waits.
- Without trust in `/hooks`, Codex does not run the hooks and shows no error in `codex exec`.

**Entries**

- A request is a question with `approve` and `reject` choices. See [Questions and approvals](reference.md#questions-and-approvals).
- A card is a `state` entry with the subject `AGENT/PROJECT`, such as `claude/yappr`. PROJECT is the name of the working directory.
- A notification is a `claude.notification` entry.
