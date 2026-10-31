# Scripting with yappr

Yappr appends entries and checks question decisions. It never executes the action described in an entry.
These rules define the scripting interface.

**stdout carries data only.** `post` prints the new entry id. `read` prints
entries. `id` and `ticket` print their values. All status messages,
warnings and errors go to stderr.

**Text is for people. JSON is for programs.** When stdout is a terminal,
`read`, `questions` and `health` print text. When stdout is a pipe or a file,
they print one JSON object per line. The text layout can change between
versions. Do not parse it. In a script, write `--output json` so that the
format does not depend on where the output goes.

| Command | Terminal | Pipe or file |
|---|---|---|
| `read` | text, one line for each one-line entry | JSON, one entry per line |
| `questions`, `health` | text | JSON, one object per line |
| `status` | text | one JSON object |
| `post`, `ask`, `answer`, `id`, `ticket` | one value | the same value |

`--output text`, `--output json` and, for `read`, `--output raw` select a
format for any destination. `--output raw` prints the bodies only.

**`read --output json` prints one entry per line** (NDJSON), with no color
and no other lines. Each entry has the format below. The feed file on the
writer has the same format.

**The entry format:**

```json
{"v":1,"id":42,"time":"2026-10-27T09:30:00Z","source":"ci","kind":"deploy.finished","level":"info","subject":"deploy/web","title":"Deployed web","body":"v1.4.2 is live.","data":{"version":"1.4.2"}}
```

| field    | type    | rule                                                        |
|----------|---------|-------------------------------------------------------------|
| `v`      | integer | schema version, now 1                                       |
| `id`     | integer | 1, 2, 3 and so on, in the order of the writer               |
| `time`   | string  | RFC 3339 time in UTC, set by the writer                     |
| `source` | string  | 1 to 64 bytes, one line                                     |
| `kind`   | string  | 1 to 32 bytes, one line. What the entry is. The default is `event` |
| `level`  | string  | optional: `info`, `warn` or `alert`. No level means `info`  |
| `subject`| string  | optional, 1 to 256 bytes, no spaces. What the entry is about, such as `backup/nas` |
| `title`  | string  | optional, at most 256 bytes, one line                       |
| `body`   | string  | at most 256 KiB. Newline and tab are the only control characters |
| `format` | string  | optional: `text` or `markdown`. No format means `text`      |
| `data`   | object  | optional JSON object for programs, at most 64 KiB           |

An entry has a title, a body or both. `post` rejects an entry that breaks a
rule.

**Kinds.** `event` means that something happened. `state` means the current
state of the subject: a newer `state` entry with the same subject replaces it.
`question` is a question or an approval request. Use your own names for other
kinds, such as `deploy.finished` or `backup.failed`. Entries from before
levels have the kind `info`, `warn` or `alert`. Readers show them as events,
and `warn` and `alert` as that level.

**Markdown.** With `--markdown`, the app shows the body as Markdown. It does
not show raw HTML or images, and it makes links from `http` and `https` URLs
only. `read` prints the Markdown source. This includes terminal color codes in the body, so use `NO_COLOR=1` on
the producer.

**Versions.** A new optional field does not change `v`. A change that removes
a field, or changes the meaning of a field, increments `v`. Ignore the fields
that you do not know. `read` keeps them in its JSON output, and it stops at
an entry with a `v` that it does not know.

**Input for `post --json`.** Each line is one JSON object. `post` uses the
fields `source`, `kind`, `level`, `subject`, `title`, `body`, `format` and
`data`. A field that a line does not have comes from the options (`--source`,
`--kind`, `--level`, `--subject`, `--title`, `--markdown`, `--data`). The writer sets `v`,
`id` and `time`, so `post` ignores these fields in the input. `post` keeps all
other fields in the entry. Thus the output of `read --output json` is valid
input. With several writers, that output wraps each entry as
`{"writer": ..., "entry": {...}}`. `post --json` takes the entry from such a
line. An entry is at most 768 KiB as a JSON line.

**Bad input lines.** With `--json`, `post` skips a line
that is not valid, writes the reason to stderr, and continues. At the end it
exits with code 4.

## Exit codes

Every command uses the same codes, so a script can branch on them.

| code | meaning | commands |
|------|---------|----------|
| 0 | success. For `approve`, the request was approved | all |
| 1 | any other error | all |
| 2 | usage error | all |
| 3 | the writer does not allow this device | `read`, `answer`, `health`, `retry` |
| 4 | an entry or a choice is not valid | `post`, `read`, `answer`, `retry` |
| 5 | the device cannot reach the writer | `read` (without `-f`), `answer`, `health`, `retry` |
| 6 | the question expired before an answer | `ask`, `approve`, `wait`, `answer`, `retry` |
| 7 | the responder rejected the request | `approve`, `wait` |
| 8 | the question was cancelled, or Ctrl-C stopped the wait | `ask`, `approve`, `wait`, `answer`, `retry` |

`read` also gives 0 on Ctrl-C, or when the reader of its output stops
(`| head`). `read -f` tries again when it cannot reach the writer, so it does
not give 5. `ask` gives 0 for every choice, also for a choice named `reject`.

In a pipeline, use the code of `approve` to decide the next step:

```sh
yappr approve ops "Deploy $COMMIT to production?" --timeout 20m
case $? in
  0) ./deploy ;;
  7) echo "Rejected" ;;
  6) echo "No answer in time" ;;
  *) exit 1 ;;
esac
```

A cron job can treat code 5 as "try later":

```
yappr read <ticket> --output json >> alerts.ndjson || [ $? -eq 5 ]
```

## Questions and choices

`yappr ask` and `yappr approve` create a question in the local writer's feed.
The first argument names one authorized device. `--to ALIAS_OR_ID` can replace it.
The second argument is the question. `--title TEXT` can replace it.
Use `--timeout DURATION` to set the deadline. The default is ten minutes.
The writer must also grant the responder read access to the feed.

`ask` accepts repeated `--choice VALUE=Label` arguments. The label is optional.
`ask` with fewer than two choices returns code 2. Use `approve` for approval.
Values use letters, digits, underscores, and hyphens. Values must be unique.
A value contains at most 64 bytes. A label contains at most 256 bytes.
Each question accepts two to twelve choices.

Without `--no-wait`, the command prints the selected value after the writer saves a decision.
With `--no-wait`, it prints the question ID and returns immediately.
Use `yappr wait ID` to read the result later.
`yappr questions --output json` prints one question state per line.
Use `yappr cancel ID` to cancel a pending question.

For an approval, the preset values are `approve` and `reject`.
The [exit codes](#exit-codes) table lists the result codes of `ask`, `approve` and `wait`.

`yappr answer WRITER ID VALUE` saves a submission locally before it contacts the writer.
On success, it prints the decision as JSON.
On a transport failure, it returns code 5. The submission remains queued.
Use `yappr retry` to send queued submissions again.

The writer accepts one final decision for each question.
A repeated submission with the same ID and value returns that saved decision.
A different submission cannot replace a final decision.
The writer rejects expired questions, cancelled questions, invalid values, and unauthorized responders.

The feed keeps question, decision, and cancellation records in its `choices` extension field.
Each record has a `type`: `question`, `decision`, or `cancelled`.
Ordinary feed readers can ignore this field.

## Pairing

`yappr init` starts serving, pairs one device, and keeps serving.
`yappr pair` pairs one device while `yappr serve` runs in another process.
Both print the pairing code to stdout and status to stderr.
When stderr is a terminal, they also draw the code as a QR code there.
Use `--timeout DURATION` to change the five-minute invitation lifetime.
Use `--name NAME` to set the writer name that the joining device shows. The default is the host name.

A pairing code starts with `YAPPR:`, followed by capital letters and digits.
It holds the token, the deadline, the writer name, and the writer's ticket.
Letter case does not matter when you type it.

An invitation carries a random token and the writer's endpoint ticket.
The first authenticated device that presents the token claims the invitation.
The writer must accept that device before it receives read access.
A different device cannot reuse the claimed invitation.

When stdin is a terminal, `pair` asks you there to accept the device.
For a process without a terminal, accept or reject the pending device in another terminal:

```sh
yappr pair --accept DEVICE_ID
yappr pair --reject DEVICE_ID
```

The `--accept` and `--reject` commands must use the same feed directory as the invitation.
After the writer accepts a device, it saves an alias from the device name, for example `my-phone`.
If another device has that alias, a number follows it, for example `my-phone-2`.
Use `--alias NAME` to choose the alias.

`yappr join CODE --name NAME` requests access from another CLI device.
It prints the writer's ticket after the writer accepts the device.
The device that claimed an invitation can read the answer for two minutes after the deadline.

## Several writers

`yappr read laptop server` reads both local aliases through one endpoint.
Each writer resumes from its own cursor.
One failed writer does not stop another writer's task.
Multiple writers add a `writer` and `entry` envelope to JSON output.
The `writer` field contains the supplied alias or the full endpoint ID.
Single-writer output keeps its existing format.
Text and raw output use a `[writer]` prefix for each entry.

Use `yappr health laptop` for a read-only connection probe.
Its result includes the writer identity, connection state, selected route, address, and round-trip time.
A denied probe returns 3. An unreachable probe returns 5.
The probe leaves all cursors unchanged.

Several feeds print entries as they arrive. They do not guarantee a shared time order.

## Reading

`post` works without a running `serve` command. Entries wait in the local feed.
`read` saves its position after each entry and resumes there after a failure.
`--since ID|DURATION|TIME` and `--tail N` do not change the saved position.
Use `--cursor NAME` for an independent position, for example one for a script.
Use `--source REGEX`, `--kind KIND`, `--not-kind KIND`, `--level LEVEL` and `--subject PATTERN` to filter output.
In `--kind`, `--not-kind` and `--subject`, `*` matches any text, as in `deploy.*` or `backup/*`.
`--level warn` prints `warn` and `alert` entries.
`--latest` prints only the newest entry for each subject from the feed history. Entries without a subject all print.
`--output raw` prints bodies only.
The writer applies these filters, so entries that the filter leaves out do not cross the network.
After the replay, the cursor moves past every entry that the writer checked, also the entries that the filter left out.

## Running a command for each entry

`yappr on WRITER [filters] -- COMMAND` runs the command once for each new entry that the filters match.
It takes the same filters as `read`.

```sh
yappr on server --kind deploy.requested -- ./deploy.sh
```

The command gets the entry as one JSON line on stdin.
Its environment also has `YAPPR_ID`, `YAPPR_KIND`, `YAPPR_LEVEL`, `YAPPR_SUBJECT` and `YAPPR_SOURCE`.
No shell reads the command or the entry, so the text of an entry cannot become a command.
`on` waits for the command, and it runs one command at a time.

The first start begins after the last entry of the feed, so `on` does not run the command for old entries.
`on` saves its position after the command exits with 0.
When the command fails, `on` stops with code 1, and the entry runs again at the next start.
With `--skip-failed`, `on` writes the error to stderr and continues with the next entry.
`on` uses its own saved position, named `on`. Use `--cursor NAME` for each handler on the same writer.
Run `on` under a service manager, such as systemd or launchd, to restart it after a failure.


## Devices and access

`yappr status` shows this machine's endpoint ID, its feed, its peers, its questions, and its outbox.
A peer is a device with an alias, or a device that can read this feed.
The "reads this feed" column tells if the peer is in this machine's allow list.
It says nothing about access to that peer's feed.
In a pipe, `status` prints one JSON object with the fields `endpoint`, `feed`, `peers`, `questions`, and `outbox`.

`yappr alias set NAME ID_OR_TICKET` saves a local name. An alias does not grant access.
Save a ticket when the alias needs explicit addresses. An ID alone depends on address discovery.
An alias cannot change to another identity until you remove it with `yappr alias remove NAME`.

A change to the allow list applies without a restart.
A running `serve` closes the open feed of a denied reader within five seconds.
The writer checks access again for each answer.
The writer accepts unknown devices for pairing only while an invitation is open.

## Relays

```sh
yappr network show
yappr network set --relay https://relay.example.com
yappr network set --no-relay
yappr network set --defaults
yappr --relay https://relay.example.com read laptop
```

`network set` saves the settings for the selected feed directory.
The global `--relay` and `--no-relay` options override them for one command.
Custom URLs replace this device's home relays. A peer can still advertise a different relay.
Configure the writers and readers with the same relays when that is your intent.
Restart a running `serve` after you change its saved settings.
`--no-relay` disables relays. A connection then needs a reachable direct path.
Relays forward live traffic. They do not store entries for offline devices.

## Storage

A feed is in `$XDG_DATA_HOME/yappr/<feed>/`, or in the platform data directory.
Use `--feed NAME` to select a feed. Use `--dir PATH` or `YAPPR_DIR` to select a directory.

Files use owner-only permissions on Unix. When yappr opens an existing file or
directory of your user, it removes access for other users. It refuses a file
of another user and a symbolic link in place of a file. Iroh encrypts
connections.
This version does not encrypt local storage.
