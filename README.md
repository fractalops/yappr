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
To build from source, use `cargo install --git https://github.com/fractalops/yappr --tag v0.1.8`.

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
Run `yappr config --name "Work Mac"` to set the writer name for new entries and notifications.

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

Yappr sends approval requests and updates from Claude Code and Codex to your phone.
Install Yappr first, then install the yappr plugin for your agent.

For Claude Code:

```sh
claude plugin marketplace add fractalops/yappr
claude plugin install yappr@yappr
```

For Codex:

```sh
codex plugin marketplace add fractalops/yappr
codex plugin add yappr@yappr
```

> [!TIP]
> Codex runs new hooks only after you trust them. Start Codex and type `/hooks`.
> Trust the Yappr hooks when Codex asks for review.

The plugin acts only when you are away from the computer. You are away when you
lock the screen, or after 2 minutes with no keyboard or mouse input. At the computer, the agent
asks in the terminal as usual, and nothing goes to your phone. If Yappr cannot tell,
such as over SSH, it acts as if you are at the computer.

When you are away, the plugin asks the device that receives this feed to approve
permission requests. Each agent has one card per project on your phone. The card
shows what the agent did, and the agent's last message when its turn ends.
Without an answer in time, the agent uses its normal terminal prompt.
If you come back to the computer first, the request on your phone stops and
the agent asks in the terminal.
Claude Code can show that prompt while Yappr waits.
The plugin also gives the agent a skill to send updates and ask you questions.

To change what the plugin does, use `yappr config`:

```sh
# Ask the phone, and wait 5 minutes for an answer
yappr config --hook-to phone --hook-timeout 5m

# Also ask before pushing, and show test results on the card
yappr config --hook-events 'permission,stop,pretooluse:Bash:git push*,posttooluse:Bash:cargo test*'

# Act also when you are at the computer
yappr config --hook-when always
```

The default events are `permission,stop`. `--hook-events` replaces this selection.
In Claude Code, the plugin options replace these settings.

> [!TIP]
> `--hook-events` accepts `event[:matcher[:command-pattern]]` entries.
> Patterns use `*` for any text and `?` for one character. Repeat an event with
> different patterns to match any of them. See [command filters](docs/reference.md#command-filters) for details.

Hook support in Yappr: ☑ supported, ☐ unsupported.

| Hook | Agent | What Yappr does |
|---|---|---|
| ☑ `PermissionRequest` | Claude Code, Codex | Shows an approval request on your phone and returns your decision to the agent |
| ☑ `Stop` | Claude Code, Codex | Shows the agent's last message on its card when its turn ends |
| ☑ `PreToolUse` | Claude Code, Codex | Asks for approval before each matching tool runs |
| ☑ `Notification` | Claude Code | Sends the agent's notification message to your phone |
| ☑ `SubagentStop` | Claude Code, Codex | Shows the subagent's last message on the card |
| ☑ `PostToolUse` | Claude Code, Codex | Adds the tool and its main argument to the card, with the command output |
| ☑ `PostToolUseFailure` | Claude Code | Adds the failed tool and its error to the card |

## Reference

The [CLI reference](docs/reference.md) covers command options, exit codes,
reading from several machines, device access, and connection settings.

## License

MIT OR Apache-2.0
