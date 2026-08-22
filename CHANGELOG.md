# Changelog

## 0.3.0 — unreleased

The store is encrypted at rest.

- The store file is now a SQLCipher database (AES-256). The recorder
  generates a random key on its first start and keeps it in the login
  Keychain as "Zanei store key"; it is not synced to iCloud Keychain
  and never leaves the Mac. The CLI, the launchd recorder, and the MCP
  server read it without dialogs, and `brew upgrade` keeps access.
- A plaintext store from an earlier version is not rewritten. On the
  first `zanei start` after upgrading, the recorder renames it to
  `store.sqlite.plaintext-<timestamp>` and starts a fresh encrypted
  store; every read keeps returning the old events next to the new ones
  until they age out of retention, then the recorder deletes the file.
  `status` lists it under `store.retired_plaintext`, and `purge` covers it.
  The set-aside file is made owner-only; one that cannot be read or purged
  is reported under `degraded.retired_store` (by `status`, the MCP
  `get_status`, and the recorder) instead of stopping the recorder.
- The store, its `-wal`/`-shm` companions, and a store directory that
  Zanei creates are owner-only (0600 / 0700).
- `status` reports `store_locked` (exit code 1) when the store is
  encrypted but cannot be opened with the key, and `status --json`
  adds `store.encryption` (`sqlcipher`, `plaintext`, or null).
- `doctor` reports where the store key is — a `Store key:` line and a
  `store_key` object in `--json` — and no longer fails on a locked
  store.
- `export --format sqlite --out FILE` writes a plaintext SQLite
  snapshot of the range with the same tables as the live store. It
  never overwrites, and the file is owner-only.
- `purge` on a store that does not exist prints `Purged 0 events`
  instead of creating one.
- `ZANEI_STORE_KEY_FILE=<path>` reads the key from a file instead of
  the Keychain, for builds from source and CI; the recorder creates the
  file and its directory when they are missing.
- SQLCipher (BSD-3-Clause) is compiled in; see `THIRD_PARTY_NOTICES.md`.

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
