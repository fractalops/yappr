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
| `init` | Create the feed and identity if needed, serve, pair one device, then keep serving |
| `serve [--port PORT]` | Serve the feed to allowed devices |
| `post` | Append entries and print their numeric IDs |
| `read WRITER...` | Read entries since the saved position, then exit |
| `on WRITER -- COMMAND...` | Run a handler for each new matching entry |
| `ask DEVICE QUESTION --choice VALUE...` | Create a question and wait for a choice |
| `approve DEVICE QUESTION` | Create an approval request and wait for a decision |
| `questions`, `wait ID`, `cancel ID` | List local questions, wait for a result, or cancel a pending question |
| `answer WRITER ID VALUE`, `retry` | Submit an answer, or resend queued submissions |
| `pair`, `join CODE` | Invite a device, or request access with a pairing code |
| `allow DEVICE`, `deny DEVICE` | Grant or revoke read access |
| `alias set NAME ID_OR_TICKET`, `alias remove NAME` | Save or remove a local device name |
| `id`, `ticket` | Print the local endpoint ID or feed ticket |
| `status`, `health WRITER` | Inspect local state or probe a writer's connection |
| `network show`, `network set` | Read or save relay settings |

## Output

stdout carries command results. Status messages, warnings, and errors go to stderr.

| Command | stdout |
|---|---|
| `post` | One numeric entry ID per saved entry |
| `ask`, `approve`, `wait` | Selected value, or question ID with `--no-wait` on creation |
| `answer`, `retry` | One JSON decision per successful submission |
| `read` | Entries |
| `questions`, `health`, `status` | Question states, connection health, or local state |
| `init`, `pair` | Pairing code |
| `join` | Writer's ticket after acceptance |
| `id`, `ticket`, `network show` | Endpoint ID, ticket, or JSON relay settings |
| `on` | Handler output |

`read`, `questions`, `health`, and `status` default to text on a terminal and JSON in a pipe or file.
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

Without `--body`, `post` reads stdin until EOF and saves one entry. It works without a running `serve` process.
Use `--json` for one entry per JSON input line.

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

`event` records an occurrence. `state` records the current state of a subject.
The app shows the newest `state` per subject per writer. The log retains all entries.
`question` carries a question or approval request. Custom kinds, such as `backup.failed`, are valid.
Readers treat legacy kinds `info`, `warn`, and `alert` as events with the corresponding level.

`--markdown` selects Markdown bodies. The app disables raw HTML and images, and permits only HTTP and HTTPS links.
The CLI prints Markdown source. Bodies cannot contain terminal escape codes. Disable color in programs that supply body text.

### JSON input

`post --json` accepts the fields above and preserves unknown fields. Missing or null input fields use the command's defaults.
The writer assigns `v`, `id`, and `time`, ignoring input values for those fields.
It also accepts the multi-writer envelope and extracts `entry`.

```sh
yappr read laptop --output json | yappr post --json
```

This copies entries into the local feed with new IDs and timestamps.
An invalid input line produces an error on stderr. Processing continues, and the final exit code is 4.

### Schema versions

Optional fields retain the current `v`. Removing a field or changing its meaning increments `v`.
Readers should ignore unknown fields. `read` preserves them in JSON and stops at an unsupported schema version.

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

Kind and subject patterns support `*` for any text. Other characters are literal. Different filters apply together.
`--latest` retains every entry without a subject. With `-f`, all later matching entries print as they arrive.

Each writer has a separate cursor. `read` saves its position after printing an entry.
After replay, the cursor also advances past entries excluded by filters. Changing filters does not restore those entries.
`--since` and `--tail` leave the saved cursor unchanged. Use `--since 0` to replay the full history.
The writer filters entries before transmission.

Several writers share one endpoint. One failed writer does not stop the others.
Entries print in arrival order across writers.
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

Both commands create a local question for one allowed responder. The writer must be serving for that responder to read and answer.
`--to DEVICE` and `--title QUESTION` can replace the positional arguments.
Both accept `--body`, `--source`, `--level`, `--subject`, `--data`, and `--markdown`.
`--timeout DURATION` sets the deadline. Default: `10m`.

`ask` requires 2–12 unique choices. Repeat `--choice VALUE[=LABEL]` for each choice.
Values contain 1–64 ASCII letters, digits, underscores, or hyphens. Labels contain 1–256 bytes on one line.
Omitting a label uses its value. `approve` supplies `approve` and `reject`.

By default, creation waits and prints the selected value after the writer saves the decision.
`--no-wait` prints an opaque question ID immediately. This differs from the numeric entry ID.
Use that question ID with `wait`, `cancel`, or `answer`:

```sh
id=$(yappr approve phone "Deploy to production?" --no-wait)
yappr wait "$id"
```

On the responder, use `yappr answer WRITER ID VALUE`.
It saves the submission locally before contacting the writer. A transport failure returns 5 and leaves the submission queued.
`retry` resends queued submissions.

The writer accepts one final decision. Repeating the same submission returns the saved decision.
A different submission cannot replace it. The writer rejects expired or cancelled questions, invalid values, and unauthorized responders.
Ctrl-C during `ask` or `approve` attempts cancellation. An already final decision determines the result.

`questions --output json` prints states with `question`, `status`, and `decision` fields.
Statuses are `pending`, `answered`, `expired`, or `cancelled`.
Feed records use the `choices` extension with type `question`, `decision`, or `cancelled`.

## Pairing and access

`init` serves and pairs one device. `pair` requires a separate running `serve` process.
Both print a `YAPPR:` pairing code to stdout and a QR code when stderr is a terminal.
Codes ignore letter case and contain the writer's ticket, name, token, and deadline.

| Option | Purpose | Default |
|---|---|---|
| `--timeout DURATION` | Invitation lifetime | `5m` |
| `--name NAME` | Writer name shown during pairing | Host name |
| `--alias NAME` | Local alias for the joining device | Derived from its name |

The first authenticated device with the random token claims the invitation. The writer must accept it before granting access.
With terminal input, the writer prompts for acceptance. Otherwise, use another terminal with the same feed directory:

```sh
yappr pair --accept DEVICE_ID
# Or reject it:
yappr pair --reject DEVICE_ID
```

A CLI reader joins with `yappr join CODE --name NAME [--alias NAME]`.
Here, `--name` names the joining device and `--alias` names the writer locally.
Acceptance must occur before expiry. The claiming device can retrieve the result for two minutes after the deadline.
`init` keeps serving after pairing ends, including expiry or rejection.

Aliases name devices locally. They do not grant read access.
An alias holding only an ID depends on address discovery. Save a ticket to retain explicit addresses.
Remove an alias before assigning it to another identity.
`status` lists peers. Its read-access flag describes access to this local feed.

Allow-list changes apply without restarting. `deny` closes active feed connections within five seconds.
The writer checks access on each answer. Unknown devices can reach pairing only while an invitation is pending.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Other error, including handler failure |
| 2 | Usage error |
| 3 | Writer denied access |
| 4 | Invalid entry, question, or choice |
| 5 | Writer unreachable |
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
The identity persists in `secret.key`. `IROH_SECRET` overrides that key.

```sh
yappr network show
yappr network set --relay https://relay.example.com
yappr network set --no-relay
yappr network set --defaults
```

`network set` saves settings for this feed directory. Global relay flags override them for one command.
Restart `serve` after changing saved settings. Custom URLs replace this device's home relays. Peers can advertise other relays.
`--no-relay` requires a reachable direct path. Relays forward encrypted live traffic and cannot read it.
They do not store entries for offline devices.

Unix files and directories use owner-only permissions. Yappr tightens permissions on existing files owned by your user.
It rejects files owned by other users and symbolic links in place of files.
Iroh encrypts connections. Yappr stores local data without encryption.
