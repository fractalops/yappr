# Scripting with yappr

Yappr appends entries and checks question decisions. It never executes the action described in an entry.
These rules define the scripting interface.

**stdout carries data only.** `post` prints the new entry id. `follow` prints
entries. `id`, `ticket` and `devices` print their values. All status messages,
warnings and errors go to stderr.

**Single-writer `--output json` prints one entry per line** (NDJSON), with no color and no
other lines. Each entry has the format below. The feed file on the writer has
the same format.

**The entry format:**

```json
{"v":1,"id":42,"time":"2026-10-27T09:30:00Z","source":"disk","kind":"info","title":"disk usage","body":"..."}
```

| field    | type    | rule                                                        |
|----------|---------|-------------------------------------------------------------|
| `v`      | integer | schema version, now 1                                       |
| `id`     | integer | 1, 2, 3 and so on, in the order of the writer               |
| `time`   | string  | RFC 3339 time in UTC, set by the writer                     |
| `source` | string  | 1 to 64 bytes, one line                                     |
| `kind`   | string  | 1 to 32 bytes, one line. Usual values: `info`, `warn`, `alert` |
| `title`  | string  | optional, at most 256 bytes, one line                       |
| `body`   | string  | at most 256 KiB. Newline and tab are the only control characters |

An entry has a title, a body or both. `post` rejects an entry that breaks a
rule. This includes terminal color codes in the body, so use `NO_COLOR=1` on
the producer.

**Versions.** A new optional field does not change `v`. A change that removes
a field, or changes the meaning of a field, increments `v`. Ignore the fields
that you do not know. `follow` keeps them in its JSON output, and it stops at
an entry with a `v` that it does not know.

**Input for `post --json`.** Each line is one JSON object. `post` uses the
fields `source`, `kind`, `title` and `body`. A field that a line does not have
comes from the options (`--source`, `--kind`, `--title`). The writer sets `v`,
`id` and `time`, so `post` ignores these fields in the input. `post` keeps all
other fields in the entry. Thus the output of `follow --output json` is valid
input. An entry is at most 768 KiB as a JSON line.

**Bad input lines.** With `--lines`, `--every` or `--json`, `post` skips a line
that is not valid, writes the reason to stderr, and continues. At the end it
exits with code 4.

**Exit codes:**

| code | meaning                                                         |
|------|-----------------------------------------------------------------|
| 0    | success. `follow` also gives 0 on Ctrl-C, or when the reader of its output stops (`\| head`) |
| 1    | any other error                                                 |
| 2    | usage error                                                     |
| 3    | the writer does not allow this follower                         |
| 4    | an entry is not valid (input to `post`, or an entry that `follow` received) |
| 5    | the follower cannot reach the writer. `follow` tries again, so only `--no-follow` gives 5 |

For example, a cron job can treat code 5 as "try later":

```
yappr follow <ticket> --no-follow --output json >> alerts.ndjson || [ $? -eq 5 ]
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
`yappr questions` prints one question state per JSON line.
Use `yappr cancel ID` to cancel a pending question.

A generic choice named `reject` still returns code 0.
For an approval, the preset values are `approve` and `reject`.
Approval rejection returns code 7. `wait` preserves this approval behavior.

| Code | Additional meaning |
|---|---|
| 6 | The question expired. |
| 7 | The responder rejected an approval request. |
| 8 | The writer cancelled the question, or the question creator stopped its wait with Ctrl-C. |

`yappr answer WRITER ID --choice VALUE` saves a submission locally before it contacts the writer.
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

Start `yappr serve` before you run `yappr pair`.
The pairing command prints the invitation to stdout. It prints status to stderr.
Use `--expires DURATION` to change the five-minute invitation lifetime.

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
`yappr join INVITATION --name NAME` requests access from another CLI device.
It prints the writer's ticket after the writer accepts the device.

## Several writers

`yappr follow laptop server` follows both local aliases through one endpoint.
Each writer resumes from its own cursor.
One failed writer does not stop another writer's task.
Multiple writers add a `writer` and `entry` envelope to JSON output.
The `writer` field contains the supplied alias or the full endpoint ID.
Single-writer output keeps its existing format.
Pretty and raw output use a `[writer]` prefix for each entry.

Use `yappr health laptop` for a read-only JSON connection probe.
Its result includes the writer identity, connection state, selected route, address, and round-trip time.
A denied probe returns 3. An unreachable probe returns 5.
The probe leaves all cursors unchanged.

Several feeds print entries as they arrive. They do not guarantee a shared time order.

## Following

`post` works without a running `serve` command. Entries wait in the local feed.
`follow` saves its position after each entry and resumes there after a failure.
`--since ID|DURATION|TIME` and `--tail N` do not change the saved position.
Use `--cursor NAME` for an independent position, for example one for a script.
Use `--source REGEX` and `--kind KIND` to filter output. `--output raw` prints bodies only.

## Devices and access

`yappr devices` prints one device per line: alias, endpoint ID, and `yes` or `no`.
The last column tells if the device can read this machine's feed.
It says nothing about access to that device's feed.
Allowed devices without an alias show `-` as the alias.
A terminal gets a header and short IDs. A pipe gets tab-separated full IDs.

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
yappr --relay https://relay.example.com follow laptop
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

Files use owner-only permissions on Unix. Iroh encrypts connections.
This version does not encrypt local storage.
