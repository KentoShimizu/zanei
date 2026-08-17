# Changelog

## 0.2.0 — 2026-08-18

Typed-text capture now works in Electron and Chromium apps.

- With content capture on, the recorder activates each app's
  accessibility tree (AXManualAccessibility), so apps like Claude,
  ChatGPT, Slack, and VS Code expose their input fields and typed text
  lands in `ui.value` like everywhere else. Password-field exclusion is
  unchanged. Copied text was already captured regardless.
- macOS permission dialogs are requested strictly one at a time: the
  next request waits until the previous one is granted, and the event
  tap waits for the permission worker, so a second dialog can no longer
  replace the first.
- `start` waits for the recorder's own permission verdict before
  describing it, instead of claiming dialogs are pending on machines
  where everything is already granted.
- The Homebrew formula registers `Zanei.app` with LaunchServices at
  install, so `tccutil reset <service> dev.zanei.recorder` works for
  the whole time the app is installed; the docs put permission removal
  before uninstalling.
- The agent skill is half the size, points agents at `clipboard.copy`
  bodies, and explains which apps produce typed-text diffs.

## 0.1.3 — 2026-08-18

- The recorder shuts itself down within about 15 seconds when its
  executable disappears, so `brew uninstall` (or an upgrade replacing
  the Cellar path) no longer leaves a recorder running from memory.
- The FAQ documents uninstalling, deleting recorded data, and removing
  the macOS permission grants.

## 0.1.2 — 2026-08-18

- The first-run text-content question keys off the recorder's own
  permission report, never the CLI's local probe (which reflects the
  terminal's permissions). The report survives a stop, so the question
  arrives right after permissions are granted — and asks after startup,
  restarting the recorder, when that is the first moment everything is
  granted.
- Documentation reflects that the Input Monitoring dialog may not
  appear even when requested; `zanei doctor --fix` is the reliable
  path.

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
