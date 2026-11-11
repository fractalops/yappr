---
name: yappr-usage
description: Use when the user asks for updates or questions through yappr, or is away from the desk while an agent works. Also use when an agent needs a decision from a user who reads yappr on another device.
---

# Using yappr from a coding agent

yappr sends entries and questions from this machine to the user's other devices. Use it so the user can follow your work and answer you from the phone or from this terminal, whichever they are at.

## 1. Check the setup first

Run these quietly:

```sh
which -a yappr && yappr --version   # several binaries: use the newest by its full path in every call
yappr status                        # a running writer, and the paired devices
yappr hooks status                  # are tool permission prompts already sent to a device?
```

The device to use is the peer with `yes` under "receives entries". If more than one peer has `yes`, ask the user which one.

If no writer runs or no device is paired, stop. Tell the user what you found and what you suggest, and wait for a yes. Never install, start or pair without that yes.

- To install, use the command in the yappr README.
- To pair, the user runs `yappr start` in their own terminal and leaves it open. Do not run it in the background: it shows a QR code and asks `y/N`. On the phone, the user opens https://yappr.run, selects **Setup**, scans the code, compares the check codes, and enters `y` in their terminal. The alias comes from the device name.
- After setup, send one test post and ask the user if it arrived.
- If no hooks are installed, you can suggest `yappr hooks install claude --to <alias>` (or `codex`). Do not run it without a yes: it changes the agent settings.

## 2. Post updates

Post at milestones only: started, checks pass, blocked, done.

```sh
yappr post --markdown --source claude --kind progress --subject pairing-relay \
  --title "Relay fix committed locally" --body "$(cat <<'EOF'
Commit `a68ab53` on `test/history-timing`. Not pushed.

- The late-relay test passes. The old code fails it.
- Next: open [PR #9](https://github.com/fractalops/yappr.run/pull/9).
EOF
)"
```

| Part | Content |
|---|---|
| `--markdown` | Always, on `post`, `ask` and `approve`. Without it, the app shows plain text. |
| `--title` | Short and plain. No backticks. |
| `--body` | A one-line summary, then the full content. Say what is committed or pushed, and what comes next. Use a quoted heredoc (`<<'EOF'`) so backticks stay text. |
| `--kind` | `progress`, `done`, or `note` (a decision to keep). |
| `--subject` | The topic of the task. Use the same value for all its posts and questions. |
| `--level` | `warn` when you are blocked, `alert` when the user must act now. Otherwise leave it out. |

In the body:

- Links as `[text](url)`. A bare URL is never correct.
- Every identifier in backticks: commands, functions, file paths, branch names, commit hashes, versions, API names, UI strings.
- Output with more than one line in a fenced code block.
- The full content, such as all review findings. Never write "see the terminal".

## 3. Ask so the user can answer from either place

Do not ask only in the terminal. Do not block on `yappr ask` either, because then the user cannot answer here.

1. Send the question without waiting. It prints the question ID. Use that literal ID in the next steps, because shell variables do not persist between tool calls.
   ```sh
   yappr ask --no-wait <alias> "Push and merge PR #9?" --markdown --source claude \
     --subject <subject> --level warn --timeout 1h \
     --body "$(cat <<'EOF'
   ...the full context...
   EOF
   )" --choice merge="Push and merge" --choice wait="Wait"
   ```
   For a yes or no, use `yappr approve --no-wait`. The question body holds the full context, so no separate post is necessary. Use `--timeout 1h` when the user is away. The default is `10m`.
2. Run `yappr wait <id>` as a background task. Ask the same question in the terminal with its choices, end with "Answer here or on your phone", and end your turn.
3. Use the first answer that arrives:
   - `wait` prints the chosen value and exits 0. A rejected `approve` exits 7.
   - If the user answers in the terminal, run `yappr cancel <id>` so the phone stops showing the question.
4. Exit 6 means the question expired, and 8 means it was cancelled. Silence is never approval: do not do the action. Post one `--level warn` entry that says what is on hold, say the same in the terminal, and stop.

Tool permission prompts are the job of the hooks, not of this skill.
