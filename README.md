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
To build from source, use `cargo install --git https://github.com/fractalops/yappr --tag v0.1.7`.

## Connect your phone

On your computer, start Yappr and leave this terminal open:

```sh
yappr start
```

<img src="docs/assets/start.gif" alt="Yappr start shows a QR code and asks you to accept a device if the check codes match" width="800" />

The first start shows a QR code and a `YAPPR:` code. They expire after five minutes.

On your phone:

1. Open [yappr.run](https://yappr.run/) and select **Setup**.
2. Scan the QR code. You can also import a QR image or paste the `YAPPR:` code.
3. Set **Your device name** to `phone`, then select **Request pairing**.
4. Compare the check code on your phone with the check code in the computer's terminal.

If the codes match, enter `y` in that terminal. If they differ, enter `n`.
The computer prints the saved alias. Use that alias in the examples below.

In a second terminal on the same computer, send your first update:

```sh
echo "Backup complete" | yappr post --source backup
```

The command prints an entry number. On your phone, select **Feed** to see
**Backup complete**.

To receive notifications when Yappr is closed, open **Setup** in the app.
In **Advanced settings**, select **Notify me when this page is closed**.
Allow browser notifications. Yappr sends notifications for questions, warnings, and alerts.
Run `yappr push --preview title` to show only the title in notifications from this feed.
Run `yappr push --name "Work Mac"` to set the writer name for new entries and notifications.
Run `yappr push --name ""` to use the computer name again.

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
# Install approval and turn-end hooks
yappr hooks install claude
yappr hooks install codex

# Ask before pushing and send test results
yappr hooks install claude \
  --events 'permission,stop,pretooluse:Bash:git push*,posttooluse:Bash:cargo test*'

# Include subagent and session updates for this project
yappr hooks install codex --scope project --events permission,stop,subagent,session

# Remove Yappr hooks
yappr hooks remove claude
yappr hooks remove codex
```

The default events are `permission,stop`. `--events` replaces this selection.

> [!TIP]
> `--events` accepts `event[:matcher[:command-pattern]]` entries.
> Patterns use `*` for any text and `?` for one character. Repeat an event with
> different patterns to match any of them. See [command filters](docs/reference.md#command-filters) for details.

> [!TIP]
> Codex runs new hooks only after you trust them. Start Codex, type `/hooks`, and
> trust the yappr hooks.

Hook support in Yappr: ☑ supported, ☐ unsupported.

| Hook | Agent | What Yappr does |
|---|---|---|
| ☑ `PermissionRequest` | Claude Code, Codex | Shows an approval request on your phone and returns your decision to the agent |
| ☑ `Stop` | Claude Code, Codex | Sends the agent's last message when its turn ends |
| ☑ `PreToolUse` | Claude Code, Codex | Asks for approval before each matching tool runs |
| ☑ `Notification` | Claude Code | Sends the agent's notification message to your phone |
| ☑ `SubagentStop` | Claude Code, Codex | Sends an update when a subagent finishes |
| ☑ `SessionStart`, `SessionEnd` | Claude Code, Codex | Sends an update when a session starts or ends |
| ☑ `PostToolUse` | Claude Code, Codex | Sends a completion update with the tool name and a result summary |
| ☑ `PostToolUseFailure` | Claude Code | Sends an update with the tool name and error when a tool fails |

## Reference

The [CLI reference](docs/reference.md) covers command options, exit codes,
reading from several machines, device access, and connection settings.

## License

MIT OR Apache-2.0
