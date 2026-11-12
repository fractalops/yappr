# CLI reference

Use `yappr COMMAND --help` for all options and examples. Uppercase names below are placeholders.

- [Commands](#commands)
- [Output](#output)
- [Entries and posting](#entries-and-posting)
- [Reading](#reading)
- [Handlers](#handlers)
- [Questions and approvals](#questions-and-approvals)
- [Pairing and access](#pairing-and-access)
- [Exit codes](#exit-codes)
- [Configuration and storage](#configuration-and-storage)

## Commands

`WRITER` accepts a local alias, endpoint ID, or feed ticket. `DEVICE` accepts an alias or endpoint ID.
Commands operate on the selected local feed unless they name a writer.

| Command | Purpose |
|---|---|
| `start [--pair] [--port PORT]` | Serve the feed and deliver it to receiving devices. The first start pairs one device |
| `post` | Append entries and print their numeric IDs |
| `read [WRITER...]` | Read entries since the saved position, then exit. Without a writer, read the local feed |
| `on WRITER -- COMMAND...` | Run a handler for each new matching entry |
| `ask DEVICE QUESTION --choice VALUE...` | Create a question and wait for a choice |
| `approve DEVICE QUESTION` | Create an approval request and wait for a decision |
| `wait ID`, `cancel ID` | Wait for the result of a local question, or cancel it |
| `answer WRITER ID VALUE`, `retry` | Submit an answer, or resend queued submissions and deliver the feed |
| `pair`, `join CODE` | Invite a device, or request access with a pairing code |
| `allow DEVICE`, `deny DEVICE` | Grant or revoke read access |
| `alias set NAME ID_OR_TICKET`, `alias remove NAME` | Save or remove a local device name |
| `id`, `ticket` | Print the local endpoint ID or feed ticket |
| `status`, `health WRITER` | Inspect local state or probe a writer's connection |
| `config` | Show or save the settings for this feed |

## Output

stdout carries command results. Status messages, warnings, and errors go to stderr.

| Command | stdout |
|---|---|
| `post` | One numeric entry ID per saved entry |
| `ask`, `approve`, `wait` | Selected value, or question ID with `--no-wait` on creation |
| `answer`, `retry` | One JSON decision per successful submission |
| `read` | Entries |
| `health`, `status` | Connection health or local state |
| `start`, `pair` | Pairing code |
| `join` | Writer's ticket after acceptance |
| `id`, `ticket` | Endpoint ID or ticket |
| `config` | Feed settings in text or JSON |
| `on` | Handler output |

`read`, `health`, and `status` default to text on a terminal and JSON in a pipe or file.
Use `--output json` in scripts. Text layouts can change between versions.
JSON output uses one object per line. `status` prints one object with `endpoint`, `feed`, `peers`, `questions`, and `outbox`.
Its `questions` field counts questions by status.

With several writers, `read` wraps each JSON entry as `{"writer":"ALIAS_OR_ID","entry":{...}}`.
Text and raw output prefix each entry with `[writer]`. Single-writer JSON output contains the entry directly.
`read --output raw` prints bodies. `--color auto|always|never` controls text color. In `auto`, `NO_COLOR` disables color.

## Entries and posting

```sh
yappr post --source ci --kind deploy.finished --subject deploy/web \
  --title "Deployed web" --body "v1.4.2 is live." --data '{"version":"1.4.2"}'
```

Without `--body`, `post` reads stdin until EOF and saves one entry. It works without a running `start` process.
Use `--json` for one entry per JSON input line.

### Delivery

A device that receives entries, such as the app, gives its address when it pairs. The writer then opens the connection and delivers the feed to that device.
A running `start` delivers each new entry. Without `start`, `post` delivers the new entries itself and waits up to 20 seconds for each device to confirm them.
If a device does not confirm, the entry stays in the feed, and `post` returns 5. The next delivery from `start`, `post`, or `retry` sends it.
The device can still receive a push notification. The notification does not confirm that the device saved the entry.

```json
{"v":1,"id":42,"time":"2026-10-27T09:30:00Z","source":"ci","kind":"deploy.finished","level":"info","subject":"deploy/web","title":"Deployed web","body":"v1.4.2 is live.","data":{"version":"1.4.2"}}
```

The feed file and single-writer `read --output json` use this format.
Limits count UTF-8 bytes. An entry needs a title, body, or both. The complete JSON line cannot exceed 768 KiB.

| Field | Type | Rule |
|---|---|---|
| `v` | integer | Schema version, currently 1 |
| `id` | integer | Writer's sequence: 1, 2, 3, … |
| `time` | string | Writer's UTC timestamp, RFC 3339 |
| `source` | string | 1–64 bytes, one line. Default: host name |
| `kind` | string | 1–32 bytes, one line. Default: `event` |
| `level` | string | Optional: `info`, `warn`, `alert`. Default: `info` |
| `subject` | string | Optional: 1–256 bytes, no whitespace, such as `backup/nas` |
| `title` | string | Optional: at most 256 bytes, one line |
| `body` | string | At most 256 KiB. Only newline and tab may be control characters |
| `format` | string | Optional: `text` or `markdown`. Default: `text` |
| `data` | object | Optional: JSON object, at most 64 KiB when serialized |
| `sig` | string | Writer's Ed25519 signature, hex. Entries from older versions have none |

`event` records an occurrence. `state` records the current state of a subject.
The app shows the newest `state` per subject per writer. The log retains all entries.
`question` carries a question or approval request. Custom kinds, such as `backup.failed`, are valid.
Readers treat legacy kinds `info`, `warn`, and `alert` as events with the corresponding level.

`--markdown` selects Markdown bodies. The app disables raw HTML and images, and permits only HTTP and HTTPS links.
The CLI prints Markdown source. Bodies cannot contain terminal escape codes. Disable color in programs that supply body text.

### JSON input

`post --json` accepts the fields above and preserves unknown fields. Missing or null input fields use the command's defaults.
The writer assigns `v`, `id`, `time`, and `sig`, ignoring input values for those fields.
It also accepts the multi-writer envelope and extracts `entry`.

```sh
yappr read laptop --output json | yappr post --json
```

This copies entries into the local feed with new IDs and timestamps.
An invalid input line produces an error on stderr. Processing continues, and the final exit code is 4.

### Schema versions

Optional fields retain the current `v`. Removing a field or changing its meaning increments `v`.
Readers should ignore unknown fields. `read` preserves them in JSON and stops at an unsupported schema version.

### Signatures

The writer signs each new entry with its identity key. The signed data starts with `yappr/entry/1`, a zero byte, the log name, and a zero byte. Then comes the entry as JSON, with sorted keys, no spaces, and no `sig` field.
The log name is the `time` of the log's first entry. `read` stops at an entry with a signature that does not match, and returns 4. The same rule applies to the local feed.
A feed belongs to the identity that signed its first entry. `post` with another identity fails.

## Reading

```sh
yappr read laptop nas -f --output json
yappr read laptop --since 0 --kind 'deploy.*' --level warn
```

| Option | Behavior |
|---|---|
| `-f`, `--follow` | Wait for new entries and reconnect after connection loss |
| `--since ID\|DURATION\|TIME` | Read after an entry ID, relative time such as `2h`, or RFC 3339 timestamp |
| `--tail N` | Read the last N entries |
| `--cursor NAME` | Use an independent saved position. Default: `default` |
| `--kind PATTERN` | Include matching kinds. Repeat to include more kinds |
| `--not-kind PATTERN` | Exclude matching kinds. Repeat to exclude more kinds |
| `--level LEVEL` | Minimum severity: `info` < `warn` < `alert` |
| `--subject PATTERN` | Include matching subjects |
| `--source REGEX` | Include matching sources |
| `--latest` | Replay only the newest matching entry per subject |
| `--open` | Print only the entries that still need attention |

Kind and subject patterns support `*` for any text. Other characters are literal. Different filters apply together.
`--latest` retains every entry without a subject. With `-f`, all later matching entries print as they arrive.

Each writer has a separate cursor. `read` saves its position after printing an entry.
After replay, the cursor also advances past entries excluded by filters. Changing filters does not restore those entries.
`--since` and `--tail` leave the saved cursor unchanged. Use `--since 0` to replay the full history.
If the writer's feed starts again with a new log, `read` starts at its first entry.
The writer filters entries before transmission.

Several writers share one endpoint. One failed writer does not stop the others.
Entries print in arrival order across writers.
Without a writer, `read` prints the local feed, with its own saved positions.

`--open` replays the feed and keeps the entries that still need attention:

- Questions with no final decision. From a writer, only the questions that this device can answer. On the local feed, all questions that this machine asked.
- For each subject, the newest `state` entry, if its level is `warn` or `alert`. A newer `state` entry at `info` closes it.

The other filters apply to these entries. `--open` does not move the saved position.
With `-f`, entries that become open print as they arrive. An entry that closes later does not print again.

`health WRITER` reports identity, connection state, route, address, and round-trip time without moving a cursor.

## Handlers

```sh
yappr on server --kind deploy.requested -- ./deploy.sh
```

`on` accepts the kind, exclusion, level, subject, and source filters from `read`.
It passes one JSON entry to the handler's stdin and runs handlers sequentially.
The environment includes `YAPPR_ID`, `YAPPR_KIND`, `YAPPR_LEVEL`, `YAPPR_SUBJECT`, and `YAPPR_SOURCE`.
An absent subject becomes an empty string. Kind and level use the legacy normalization described above.
Arguments pass directly to the program. A shell runs only if the command explicitly invokes one.

The first start skips existing history. Later starts resume from the saved cursor, named `on` by default.
Use a separate `--cursor NAME` for each handler on a writer.
After exit code 0, `on` saves the entry's position.
A failed handler stops `on` with code 1. The next start retries that entry.
`--skip-failed` reports the failure and advances past it.

A crash after a handler finishes can precede the cursor save. Handlers must tolerate repeated delivery.
A service manager can restart `on` after failure.

## Questions and approvals

```sh
yappr ask phone "Backup failed. What next?" --choice retry="Retry now" --choice skip
yappr approve phone "Deploy to production?" --timeout 20m
```

Both commands create a local question for one allowed responder. A responder that reads the feed needs a running `start` to read and answer.
A responder that receives the feed gets the question from `ask` or `approve` while they wait, and answers on that connection.
`--to DEVICE` and `--title QUESTION` can replace the positional arguments.
Both accept `--body`, `--source`, `--level`, `--subject`, `--data`, and `--markdown`.
`--timeout DURATION` sets the deadline. Default: `10m`.

`ask` requires 2–12 unique choices. Repeat `--choice VALUE[=LABEL]` for each choice.
Values contain 1–64 ASCII letters, digits, underscores, or hyphens. Labels contain 1–256 bytes on one line.
Omitting a label uses its value. `approve` supplies `approve` and `reject`.
If the browser shows action buttons, you can answer from the notification.
Use `yappr approve --unlock` to require approval in Yappr.
Select the notification to open the question.
**Reject** still sends the answer from the notification.
If there are too many choices for the browser, select the notification to open the question.

By default, creation waits and prints the selected value after the writer saves the decision.
`--no-wait` prints an opaque question ID immediately. This differs from the numeric entry ID.
Use that question ID with `wait`, `cancel`, or `answer`:

```sh
id=$(yappr approve phone "Deploy to production?" --no-wait)
yappr wait "$id"
```

On the responder, `yappr read WRITER --open` lists the open questions. Answer with `yappr answer WRITER ID VALUE`.
`answer`, `wait`, and `cancel` accept the entry ID of the question, such as `42`, or its question ID.
`answer` with an entry ID needs the writer to look up the question. With a question ID, it saves the answer when the writer is offline.
It saves the submission locally before contacting the writer. A transport failure returns 5 and leaves the submission queued.
`retry` resends queued submissions.

The writer accepts one final decision. Repeating the same submission returns the saved decision.
A different submission cannot replace it. The writer rejects expired or cancelled questions, invalid values, and unauthorized responders.
Ctrl-C during `ask` or `approve` attempts cancellation. An already final decision determines the result.

Feed records use the `choices` extension with type `question`, `decision`, or `cancelled`.

## Agent hooks

The yappr plugins for Claude Code and Codex run `yappr hook` for each agent event.
The [README](../README.md#coding-agents) shows how to install them.

```sh
yappr config --hook-to phone --hook-timeout 5m
yappr config --hook-events 'permission,stop,pretooluse:Bash:git push*,posttooluse:Bash:cargo test*'
yappr config --hook-when always
yappr config --hook-to "" --hook-timeout "" --hook-events ""
```

By default, the hooks act only when you are away: the screen is locked, or there was no keyboard or mouse input for 2 minutes.
At the computer, a hook gives no decision and posts nothing, so the agent asks in the terminal.
On macOS, Yappr reads the idle time and the lock state from `ioreg`. On Linux, it reads `LockedHint` and `IdleHint` of the graphical session from `loginctl`.
`IdleHint` uses the idle time of the desktop, which can be longer than 2 minutes.
Without a graphical session, such as over SSH, and on Windows, the hooks act for every selected event.
`--hook-when always` makes the hooks act at the computer too.

`--hook-to DEVICE` selects the device that answers. Without it, the hook asks the one device that receives the feed.
If several devices can answer, set `--hook-to`.
`--hook-timeout DURATION` sets how long a request waits. Default: `2m`. Maximum: `59m`.
An empty value restores the default.

In Claude Code, the plugin options **Device**, **Approval wait**, **Events**, **When**, and **Feed** replace these settings.
An empty option keeps the setting of `yappr config`.
Run `claude plugin configure yappr@yappr` to list the options.
In Claude Code, use `/plugin configure yappr@yappr` to set them.
Restart Claude Code to apply changes.
The hook also reads `YAPPR_HOOK_TO`, `YAPPR_HOOK_TIMEOUT`, `YAPPR_HOOK_EVENTS`, `YAPPR_HOOK_WHEN`, and `YAPPR_FEED`.

The plugin finds `yappr` on `PATH`, then in `~/.cargo/bin`. `YAPPR_BIN` replaces this search.
If it cannot find `yappr`, it writes an install hint to stderr and gives no decision.
Codex shows the hint as a warning when it asks for permission.
In Claude Code, start the CLI with `--debug-file FILE` to see hook stderr in that file.

To test a custom binary in the Codex CLI, start a local session with
`YAPPR_BIN=/path/to/yappr codex --no-daemon`.
A shared server can use a different environment or version from the CLI.

`--hook-events` selects the events. Default: `permission,stop`.

| Event | Agent event | What yappr does |
|---|---|---|
| `permission` | `PermissionRequest` | Asks for approval and gives the decision to the agent |
| `pretooluse:TOOL` | `PreToolUse` for a tool, such as `Bash` | Asks for approval before each matching tool runs |
| `posttooluse[:TOOL]` | `PostToolUse` | Adds a step to the activity card, with the command output |
| `posttoolusefailure[:TOOL]` | `PostToolUseFailure`, Claude Code only | Adds a failed step to the activity card, with the error |
| `stop` | `Stop` | Shows the last message of the agent on the activity card |
| `subagent` | `SubagentStop` | Shows the last message of the subagent on the activity card |
| `notification` | `Notification`, Claude Code only | Posts `claude.notification`. It repeats permission requests and stops |

The activity card is a `state` entry with the subject `AGENT/PROJECT`, such as `claude/yappr`. PROJECT is the name of the working directory.
The app shows only the newest `state` entry of a subject, so each agent has one card per project.
The card lists the last 5 steps: the tool and its main argument, such as the command or a path in the project.
Below the steps, it shows the stdout and stderr of the last command, up to 2 KiB. It does not show other results, such as the content of a file that the tool read.
After a stop, the next step starts a new list.

A request shows the tool and its command on the device. Approval allows the tool, and rejection denies it.
With no answer before the timeout, the hook gives no decision. The agent applies its normal permission rules.
Claude Code can show its terminal prompt while Yappr waits. The prompt remains available after the timeout.
A hook always exits 0 and writes errors to stderr. An error never blocks the agent.

A hook that posts saves the entry and returns. Another process delivers it. If no device is reachable, `start` or `retry` delivers it later.
Bodies longer than 8 KiB are cut.

Hook input is limited to 1 MiB. Larger events can fail decoding and produce no question or update.
Yappr reports the error on stderr and gives no approval decision.

Codex runs new or changed hooks only after you trust them with `/hooks`. Without trust, Codex skips them and shows no error in `codex exec`.

### Command filters

Each event entry uses `event[:matcher[:command-pattern]]`. The matcher is a regular expression for the whole tool name, such as `Bash` or `Edit|Write`.
For `pretooluse`, `posttooluse`, and `posttoolusefailure`, the third field filters `tool_input.command`.
`PermissionRequest` does not accept a command pattern.

```sh
yappr config \
  --hook-events 'permission,stop,posttooluse:Bash:cargo test*,posttooluse:Bash:cargo build*'
```

Patterns match the whole command text and are case-sensitive. `*` matches any text, including an empty string.
`?` matches one character. All other characters match literally. Yappr does not interpret shell syntax.

| Pattern | Matches | Does not match |
|---|---|---|
| `cargo test` | `cargo test` | `cargo test --release` |
| `cargo test*` | `cargo test`, `cargo test --release` | `cd app && cargo test` |
| `*cargo test*` | `cd app && cargo test` | `cargo build` |

A command containing the pattern as quoted text can also match. For example, `*cargo test*` matches `echo "cargo test"`.
These filters select hook actions by text. They do not identify every command that a shell can execute.

Use one matcher per event. Combine tool names with `|`, such as `Bash|Read`.
Repeat the same event and matcher to accept any listed command pattern. Overlapping patterns produce one hook action.
Yappr rejects different matchers for the same event before it saves the settings.
An entry without a command pattern accepts all calls for its matcher. Calls without a string `tool_input.command` do not match a command pattern.
Other events keep their selected behavior. `--hook-events` replaces the default selection, so include `permission,stop` to keep them.

Quote the `--hook-events` argument to protect spaces, wildcards, and `|` from the shell.
The matcher can contain regex groups and character classes. Colons inside them stay in the matcher, such as `(?:Bash|Write)` or `[a:z]`.
Escape a literal colon outside them as `\:`, such as `posttooluse:mcp__server\:tool`.
The next unescaped colon outside groups and classes starts the command pattern.
Further colons stay in the command pattern, so URLs and drive paths work.
Commas separate event entries and cannot appear inside a matcher or command pattern. Each command pattern can contain at most 4096 bytes.

`PostToolUse` adds a step to the activity card.
Claude Code uses `PostToolUseFailure` for tool failures. Codex also reports unsuccessful Bash commands through `PostToolUse`.
Yappr preserves the reported result and does not infer an exit code when the agent provides none.

## Pairing and access

The first `start` pairs one device. `start --pair` pairs a device also when other devices can read the feed.
`pair` pairs a device while `start` runs in another terminal. Each prints a `YAPPR:` pairing code to stdout and a QR code when stderr is a terminal.
Codes ignore letter case and contain the writer's ticket, name, token, and deadline.

| Option | Purpose | Default |
|---|---|---|
| `--timeout DURATION` | Invitation lifetime | `5m` |
| `--name NAME` | Writer name shown during pairing | Host name |
| `--alias NAME` | Local alias for the joining device | Derived from its name |

The first authenticated device with the random token claims the invitation. The writer must accept it before granting access.
Compare the six-digit `Check code` on both devices.
With terminal input, enter `y` if they match. Enter `n` if they differ.
With input from a pipe or when terminal input ends, stderr shows the accept command.
Run the accept or reject command on the writer in another terminal with the same feed directory:

```sh
yappr pair --accept DEVICE_ID
# Or reject it:
yappr pair --reject DEVICE_ID
```

A CLI reader joins with `yappr join CODE --name NAME [--alias NAME]`.
Here, `--name` names the joining device and `--alias` names the writer locally.
Acceptance must occur before expiry. The claiming device can get the result for two minutes after the deadline.
`start` keeps serving after pairing ends, including expiry or rejection.
A device that receives entries, such as the app, gives its address with its request. After acceptance, the writer delivers its feed to that address. It keeps only the device ID and its own relays. The device cannot choose other hosts for the writer to contact. See [Delivery](#delivery).

Aliases name devices locally. They do not grant read access.
An alias holding only an ID depends on address discovery. Save a ticket to retain explicit addresses.
Remove an alias before assigning it to another identity.
`status` shows the running writer’s process ID, version, and path. It also lists peers. The read-access flag shows access to this local feed. Its receive flag shows the devices that get deliveries.

Allow-list changes apply without restarting. `deny` closes active feed connections within five seconds.
The writer verifies access on each answer. Unknown devices can reach pairing only while an invitation is pending.

### When start must run

Keep `yappr start` running so devices can catch up when they reconnect.
A CLI reader needs it for live reads and confirmed answers.

Without `start`, `post` can deliver entries to an open browser.
`ask`, `approve`, and `wait` can deliver questions and receive answers while they wait.
`retry` can deliver saved entries and resend queued answers.
See [Delivery](#delivery) for confirmation timeouts and retry behavior.

### Connect another CLI

On the computer that writes the feed, create a pairing code:

```sh
yappr start --pair
```

Leave that terminal open. If `start` already runs, use `yappr pair` in another terminal instead.

On the receiving computer, join with that code:

```sh
yappr join YAPPR:... --name laptop --alias computer
```

Replace `YAPPR:...` with the code from the writing computer.
Both terminals show the same six-digit code, such as `881 331`.
If the codes match, enter `y` in the writing computer's terminal.
It saves the receiving device as `laptop`. The receiving device saves the writer as `computer`.

On the receiving computer, read the feed and wait for new entries:

```sh
yappr read computer -f
```

In a second terminal on the writing computer, post an entry:

```sh
echo "Backup complete" | yappr post --source backup
```

The receiving computer prints the entry. It resumes after sleep or connection loss.
To list open questions there, use `yappr read computer --open`.
To answer entry 42, use `yappr answer computer 42 approve`.
Transport failures leave queued answers for `yappr retry`.

### Inspect and manage devices

On the computer that writes the feed, inspect the paired devices and their access:

```sh
yappr status
```

Pairing grants access. To allow a known device separately, use:

```sh
yappr allow phone
```

To revoke that device's access, use:

```sh
yappr deny phone
```

On a receiving CLI, inspect the writer's connection without advancing its saved position:

```sh
yappr health computer
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Other error, including handler failure |
| 2 | Usage error |
| 3 | Writer denied access |
| 4 | Invalid entry, question, or choice |
| 5 | Writer unreachable, or a receiving device did not confirm a new entry |
| 6 | Question expired |
| 7 | Approval rejected |
| 8 | Question cancelled, or creation interrupted while waiting |

`approve` and `wait` return 7 for a rejected approval. `ask` returns 0 for every accepted choice, including a value named `reject`.
`read -f` retries connection failures. `read` and `on` return 0 on Ctrl-C.
A closed output pipe also returns 0.

```sh
yappr approve phone "Deploy to production?" --timeout 20m
case $? in
  0) ./deploy ;;
  6) echo "Expired" ;;
  7) echo "Rejected" ;;
  8) echo "Cancelled" ;;
  *) exit 1 ;;
esac
```

## Configuration and storage

| Global option | Environment | Default |
|---|---|---|
| `--feed NAME` | `YAPPR_FEED` | `default` |
| `--dir PATH` | `YAPPR_DIR` | Selected feed's directory |
| `--no-mdns` | | Local discovery enabled |
| `--relay URL` (repeatable) | | Saved relays, otherwise n0 defaults |
| `--no-relay` | | Relays enabled |

Feeds use `$XDG_DATA_HOME/yappr/FEED/` or the platform data directory. `--dir` overrides feed selection.
The private key persists in `secret.key`. `YAPPR_KEY` overrides that key. The older name `IROH_SECRET` also works.

```sh
yappr config
yappr config --output json
yappr config --name "Work Mac"
yappr config --name ""
yappr config --relay https://relay.example.com --relay https://relay2.example.com
yappr config --default-relays
yappr config --no-relay
yappr config --no-mdns
yappr config --mdns
yappr config --hook-to phone --hook-timeout 5m --hook-events permission,stop
```

`config` shows the settings for this feed. You can save more than one setting in one call.
On other commands, `--relay`, `--no-relay`, and `--no-mdns` apply to that run.
An empty writer name uses the computer name. `start --name` and `pair --name` set the name for one invitation.
Restart `start` after a change to the relay or local discovery settings.
The `--hook-*` options are described in [Agent hooks](#agent-hooks).
Custom URLs replace this device's home relays. Peers can advertise other relays.
`--no-relay` requires a reachable direct path. Relays forward encrypted live traffic and cannot read it.
They do not store entries for offline devices.

Unix files and directories use owner-only permissions. Yappr tightens permissions on existing files owned by your user.
It rejects files owned by other users and symbolic links in place of files.
Iroh encrypts connections. Yappr stores local data without encryption.
