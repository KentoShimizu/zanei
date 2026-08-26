# Changelog

## Unreleased

## 0.4.0 — 2026-08-26

Privacy defaults, event and diagnostic schemas, agent setup, and AX health
reporting changed.

- **Privacy default behavior change:** the default `filter.redactors` is now
  `["credit_card", "token"]`; email redaction remains available when explicitly
  configured. Users whose TOML explicitly sets `redactors = [...]` are
  unaffected because that value overrides the merged default. Users whose
  configuration omits `redactors` and relies on the merged default will now
  record email addresses instead of replacing them with `[REDACTED:email]`.

- **Doctor JSON contract change:** permission requirements are now expressed as
  platform-neutral capabilities (`read_accessibility_tree`, `observe_input`, and
  `automate_browser`), with macOS permission names, native states, settings URLs,
  and the Chrome bundle id nested under each capability's `detail`. The old
  `permissions`, `missing_required`, and `settings_pane` fields were removed.
  Capability states are `available`, `action_required`, and `deferred`. Human
  output, exit codes, and `doctor --fix` behavior are unchanged.
- Recorder capability snapshots use store schema v7. Upgrading preserves events
  and daemon state while discarding only the old permission snapshot; the next
  recorder heartbeat writes the capability snapshot. Status and MCP JSON shapes
  remain unchanged. Plaintext SQLite exports expose `daemon_capabilities`
  instead of `daemon_permissions`. The migration is forward-only; rollback
  requires a pre-upgrade backup.
- `content.snapshot` v3 replaces the derived `data.complete` boolean with
  `data.cutoff`
  (`time`, `nodes`, `bytes`, `stopped`, or null), so incomplete traversals say
  which safety limit ended them. Retained v2 rows remain readable and keep their
  original shape.
- AX operation failures now report a current reason with stable phase and kind,
  the native operation and numeric code when available, and the count of
  independently unresolved sites. Recovery clears only the matching PID and
  site; cumulative collector failure counts remain monotonic.
- `zanei setup` now installs the canonical skill for pi and opencode instead of
  printing an instruction snippet. Project and user scopes use each agent's
  native skill directory, including `XDG_CONFIG_HOME` for opencode; opencode MCP
  configuration remains a documented manual step.
- Launch-agent start locks are explicitly released even when a spawned process
  inherits the descriptor, preventing a finished start from blocking the next
  one.
- Documentation now distinguishes Claude Desktop chat from Claude Code, states
  that ChatGPT cannot consume the local stdio MCP server, explains AX activation
  for every allowed app, and warns that `ui.value` diffs are not verbatim text;
  quote an eligible `content.snapshot` instead.

## 0.3.1 — 2026-08-24

Chrome capture works again, and recorder problems are now diagnosable.

- Fixed the Chrome collector crash loop. AppleScript queries addressed Chrome
  through a path alias, which macOS rejects with error -1728, and every failed
  query killed the collector worker; with the worker restarting and failing
  forever, Chrome URLs and text bodies were never captured. Queries now address
  Chrome by bundle id, and a transient failure no longer terminates the worker —
  the collector reports itself unavailable and retries on the next observation.
- Persistent collector failures are visible. `status` and `doctor` show the
  current failure reason (a fixed vocabulary plus the underlying numeric error
  code) next to cumulative per-collector failure counts, and `doctor` gained an
  evidence-based `health` section that never reports `healthy` without fresh
  evidence from the current recorder.
- The launchd recorder writes diagnostic logs beside the store
  (`<store>.daemon.stdout.log` / `<store>.daemon.stderr.log`, owner-only, never
  containing captured content). The log directory must be safely owned; unsafe
  permissions or granting ACL entries stop `start` with a precise fix.
- Typed text in apps that build their accessibility tree lazily (Chromium,
  Electron): value notifications are re-registered when a focused element later
  becomes a known text field, and Zanei now activates the web accessibility
  tree itself instead of relying on other assistive clients.
- One-shot collector startup failures (for example the Secure Input monitor)
  survive pause and resume instead of silently disappearing from `degraded`.
- The store ownership lock is released explicitly, so a spawned child process
  can no longer extend ownership past the recorder's lifetime.
## 0.3.0 — 2026-08-24

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

Content snapshots add a separate, scoped opt-in for text shown in the
frontmost window.

- `capture.content_snapshot` records `content.snapshot` events from macOS
  Accessibility. It does not take screenshots or use OCR or screen-recording
  APIs. Password subtrees, single-line input values, Secure Input periods,
  Chrome Incognito windows, and out-of-scope apps/sites are not captured.
- Configuration now accepts 18 keys: the snapshot opt-in plus four app/site
  lists under each of `[filter.text_content]` and
  `[filter.content_snapshot]`. Filter scopes hot-reload; the snapshot opt-in
  requires a restart and enabling it shows the current scope with a `[y/N]`
  confirmation. Non-TTY enablement requires `--quiet` and otherwise exits 2
  without writing.
- **Behavior change for existing content capture:** both content scopes
  exclude Safari, Firefox, Brave, Edge, Vivaldi, and Arc by default because
  their private windows cannot be identified reliably. A 0.2.x user with
  `capture.text_content = true` therefore gets null typed/copied bodies in
  those browsers by default; events and non-content facts remain.
- `zanei apps [QUERY] [--json]` lists installed, running, and recently used
  apps. Every app-list `add` now resolves display names or bundle IDs and
  saves a normalized bundle ID. A previously accepted unresolved string now
  exits 2 without writing unless `--unverified` is explicit; argument-free
  `add` provides a numbered interactive selector.
- `query` and MCP `query_events` exclude `content.*` unless `types` is
  explicit. `export` includes all event types in jsonl, JSON, and SQLite by
  default and adds `--types` for narrowing. Plaintext SQLite exports include
  snapshot bodies when selected.
- CLI `query --format json` and `export --format json` remain event arrays.
  Unknown stored event types are skipped with a stderr warning, suppressed by
  `--quiet`. MCP `query_events`, JSON timeline output, and structured MCP
  timelines expose `skipped_unknown_types` in their existing result objects.
- Timeline sessions report snapshot counts without inline bodies: Markdown
  prints `Content snapshots: N` for nonzero counts, and JSON always includes
  `content_snapshots`. CLI/MCP status exposes `capture.content_snapshot` and
  human status shows the app/site modes for both content scopes.
- `purge --types <TYPES> [--before <TIME>] [--app <NAME> | --bundle-id <ID>]`
  deletes a selected subset. It is irreversible and has no dry-run.
  `export --types` applies to every format.
- The one-time first 0.3.0 start both sets aside an older plaintext store and
  creates the encrypted live store at schema version 6. A 0.2.x binary cannot
  open that live store, and its strict config reader can reject the new bool
  and nested filter sections. Downgrade by restoring a pre-upgrade backup and
  removing those settings; never edit the schema version metadata.

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
