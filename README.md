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

The yappr plugin lets Claude Code and Codex send you updates and ask you questions when they need you.

```sh
# Claude Code
claude plugin marketplace add fractalops/yappr
claude plugin install yappr@yappr

# Codex
codex plugin marketplace add fractalops/yappr
codex plugin add yappr@yappr
```

Experimental: [agent hooks](docs/hooks-reference.md) send permission requests to your phone while you are away.

## Reference

The [CLI reference](docs/reference.md) covers command options, exit codes,
reading from several machines, device access, and connection settings.

## License

MIT OR Apache-2.0
