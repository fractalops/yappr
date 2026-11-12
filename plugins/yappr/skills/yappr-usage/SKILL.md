---
name: yappr-usage
description: "Use when the user asks for Yappr or follows progress away from the desk. Also use when the task needs a decision from a user who reads Yappr on another device."
---

# Using yappr from a coding agent

yappr sends entries and questions from this machine to the user's other devices. Use it so the user can follow your work and answer you from the phone or from this terminal, whichever they are at.

## 1. Check the setup first

Run these quietly:

```sh
yappr --version
yappr status --output text
yappr config
```

Use Yappr 0.1.7 or later. If several installs exist, use the full path to the intended binary. Use the same feed for all commands in the task.

For questions, use the alias of the intended peer with `yes` under "receives entries". If several peers qualify and the user has no preference on record, ask which one. A post goes to the feed, so every device that can read that feed can see it. Check that the feed is suitable for the content before posting.

If the CLI or a receiving peer is missing, explain the setup. Install, pair, or change settings only with the user's authorization.

- To install, see the [Yappr README](https://github.com/fractalops/yappr#install).
- To pair, have the user run `yappr start --pair` in their terminal. On the phone, open [yappr.run](https://yappr.run/), select **Setup**, scan the code, and select **Request pairing**. Compare the two check codes. Enter `y` in the terminal if they match, or `n` if they differ. Use the alias printed after pairing.
- Send a test post and check that it arrived. A paired receiver can receive while `yappr start` is stopped. Keep the receiver open for live updates and questions.
- Agent hooks are experimental and come in a separate plugin, yappr-hooks. If the user asks to send tool permission prompts to the device, see [agent hooks](https://github.com/fractalops/yappr/blob/main/docs/hooks-reference.md).

## 2. Post updates

Post at milestones only: started, checks pass, blocked, done.

```sh
yappr post --markdown --source coding-agent --kind progress --subject build-checks \
  --title "Build checks passed" --body "$(cat <<'EOF'
All checks passed.

- `cargo test --locked` passed.
- Next, run the browser tests.
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
   yappr ask --no-wait phone "Run the checks again?" --markdown --source coding-agent \
     --subject build-checks --timeout 1h \
     --body "The first run timed out during browser setup." \
     --choice retry="Run again" --choice wait="Wait"
   ```
   For a yes or no, use `yappr approve --no-wait`. Put the full context in the question body. Use `--timeout 1h` when the user needs more time. The default is `10m`. If Yappr prints an ID and reports a delivery failure, keep that ID. The question is already saved.
2. Ask the same question in the current chat and offer both ways to answer. Run `yappr wait <id>` in the background if your agent can report task completion back to the session. If it cannot, tell the user to return to the chat after answering so you can read the result.
3. Apply one decision. `wait` prints the selected value and exits 0. A rejected `approve` exits 7. If the user answers in the chat, run `yappr cancel <id>` before acting. If Yappr already saved a device answer, read it with `yappr wait <id>`. Ask the user to resolve any conflict before acting.
4. Exit 6 means the question expired, and 8 means it was cancelled. Silence is never approval: do not do the action. Post one `--level warn` entry that says what is on hold, say the same in the terminal, and stop.

The yappr plugin handles tool permission prompts.
