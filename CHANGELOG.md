# Changelog

## 0.1.1 — 2026-08-18

First-run experience fixes found during on-device QA of 0.1.0.

- macOS permission dialogs are requested one at a time (Accessibility,
  then Input Monitoring); requesting both at once lost the second
  prompt.
- The first start with every permission granted asks whether to record
  typed text and clipboard contents (y/N, default no). The answer is
  written to `config.toml` and never asked again.
- `start`, `status`, and `doctor` no longer hang or fail while
  permission dialogs are waiting for an answer.
- `doctor --fix` copies the `Zanei.app` bundle path (not the CLI
  symlink) and reveals the app in Finder so it can be dragged into a
  permission pane.

## 0.1.0 — 2026-08-17

Initial release: local, OS-level activity recording for AI agents on
macOS. App and window focus, UI interactions, typing and clipboard
facts, and Chrome URLs — captured without screenshots into a local
SQLite store with 48-hour retention, exposed through a CLI, a
read-only MCP server, and a bundled skill.
