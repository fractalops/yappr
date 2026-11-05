<h1><img src="docs/assets/yappr.svg" alt="" width="48" height="48" align="middle" /> Yappr</h1>

**Give your scripts and agents a way to reach you and get an answer.**

Yappr brings updates, questions, and approval requests from your machines to
other devices. A script can wait for your reply before it continues. Each
machine keeps its feed, so devices that were offline can catch up when they
reconnect.

Your devices connect directly where possible. Otherwise, an [iroh](https://www.iroh.computer/) [relay](https://docs.iroh.computer/concepts/relays) carries
their end-to-end encrypted traffic. The relay cannot read your entries or
answers, and it does not store your feed. You do not need an account.

## Install

On macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/fractalops/yappr/releases/latest/download/yappr-installer.sh | sh
```

On Windows, in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/fractalops/yappr/releases/latest/download/yappr-installer.ps1 | iex"
```

The installer puts `yappr` in `~/.cargo/bin`. To update, run it again.
The [releases page](https://github.com/fractalops/yappr/releases) also has the archives for each platform.
To build from source, use `cargo install --git https://github.com/fractalops/yappr --tag v0.1.2`.

## Connect your phone

On your computer, start Yappr and leave this terminal open:

```sh
yappr start
```

<img src="docs/assets/start.gif" alt="Yappr start shows a pairing code, accepts a device, and keeps the feed available" width="800" />

The first start shows a QR code and a `YAPPR:` code. They expire after five minutes.

On your phone:

1. Open [yappr.run](https://yappr.run/) and select **Setup**.
2. Scan the QR code. You can also import a QR image or paste the `YAPPR:` code.
3. Set **Your device name** to `phone`, then select **Request pairing**.
4. Compare the device ID on your phone with the ID in the computer's terminal.

If the IDs match, enter `y` in that terminal. The computer prints the saved
alias. The examples below use `phone`. Use the printed alias if it differs.

In a second terminal on the same computer, send your first update:

```sh
echo "Backup complete" | yappr post --source backup
```

The command prints an entry number. On your phone, select **Feed** to see
**Backup complete**.

To use a CLI on another computer, see [Connect another CLI](docs/reference.md#connect-another-cli).

## Updates, approvals, and choices

Run these commands on the computer you paired with your phone.

### Send a command's output

```sh
df -h / | yappr post --source disk --title "Disk usage"
```

`post` saves the command's output as one entry.

### Ask before a deploy

```sh
if yappr approve phone "Deploy to production?" --body "Commit abc123"
then
  ./deploy abc123
fi
```

Select **Approve** or **Reject** on your phone. The script runs `deploy` only
after approval.

### Choose what happens next

```sh
choice=$(yappr ask phone "Backup failed. What next?" \
  --choice retry="Retry backup" --choice stop) || exit $?

case "$choice" in
  retry) ./backup ;;
  stop)  exit 1 ;;
esac
```

`ask` prints the selected value, such as `retry`. The script stops if the
question expires, you cancel it, or an error occurs.

## Coding agents

Yappr sends updates and approval requests from Claude Code and Codex to your phone.

```sh
yappr hooks install claude
yappr hooks install codex
```

Hook support in Yappr: ☑ supported, ☐ unsupported.

| Hook | Agent | What Yappr does | Events |
|---|---|---|---|
| ☑ `PermissionRequest` | Claude Code, Codex | Shows an approval request on your phone and returns your decision to the agent | `permission` |
| ☑ `Stop` | Claude Code, Codex | Sends the agent's last message when its turn ends | `stop` |
| ☑ `PreToolUse` | Claude Code, Codex | Asks for approval before each matching tool runs | `pretooluse:Bash:git push*` |
| ☑ `Notification` | Claude Code | Sends the agent's notification message to your phone | `notification` |
| ☑ `SubagentStop` | Claude Code, Codex | Sends an update when a subagent finishes | `subagent` |
| ☑ `SessionStart`, `SessionEnd` | Claude Code, Codex | Sends an update when a session starts or ends | `session` |
| ☑ `PostToolUse` | Claude Code, Codex | Sends a completion update with the tool name and a result summary | `posttooluse:Bash:cargo test*` |
| ☑ `PostToolUseFailure` | Claude Code | Sends an update with the tool name and error when a tool fails | `posttoolusefailure:Bash:cargo test*` |

The default events are `permission,stop`. `--events` replaces this selection.

Ask before pushing, and send test results:

```sh
yappr hooks install claude \
  --events 'permission,stop,pretooluse:Bash:git push*,posttooluse:Bash:cargo test*'
```

`--events` accepts `event[:matcher[:command-pattern]]` entries.
Patterns use `*` for any text and `?` for one character. Repeat an event with
different patterns to match any of them. See [command filters](docs/reference.md#command-filters) for details.

Include subagent and session updates for a project:

```sh
yappr hooks install codex --scope project --events permission,stop,subagent,session
```

> [!TIP]
> Codex runs new hooks only after you trust them. Start Codex, type `/hooks`, and
> trust the yappr hooks.

To remove the hooks, run `yappr hooks remove claude` or
`yappr hooks remove codex`.

## Reference

The [CLI reference](docs/reference.md) covers command options, exit codes,
reading from several machines, device access, and connection settings.

## License

MIT OR Apache-2.0
