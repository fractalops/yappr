# Changelog

## 0.1.5 - 2026-11-06

- Filter tool hooks by command with `event:matcher:command-pattern`.
- Receive tool results from Claude Code and Codex with `PostToolUse`. Receive Claude Code tool errors with `PostToolUseFailure`.
- Fix approval replies that could keep the answering command open.
- Preserve hook matchers and reject conflicting selections before changing agent settings.
- Improve QR sizing across terminals and clarify the hook examples and reference.

Ask before a push, and receive test results:

```sh
yappr hooks install claude \
  --events 'permission,stop,pretooluse:Bash:git push*,posttooluse:Bash:cargo test*'
```

Known limitation: hook events above 1 MiB can fail decoding and produce no question or update.
