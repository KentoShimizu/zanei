---
name: zanei
description: Query what the user did on this computer outside the current conversation — which apps, windows, files, and web pages they used, and when (last ~48h, read-only). Reach for it whenever off-conversation context would help - resuming interrupted work, resolving vague references like "that doc I was reading" or "the error I saw", reconstructing activity before this session, or drafting standups and work logs.
---

# Zanei

Zanei records the user's activity on this machine as OS-level events — app switches, window titles, UI interactions, Chrome URLs. Everything stays in a local store and is deleted after about 48 hours by default. There are no screenshots. Typed/copied content and Accessibility text shown in windows are separate opt-ins.

## When to use

- The user resumes interrupted work ("continue where I left off", "what was I working on?")
- The user asks what they recently viewed, edited, or read ("which Stripe doc was I reading?")
- The user references something from outside this conversation that you cannot see — "that doc", "the error I saw", "the PR I reviewed this morning"
- Drafting a standup or work log from the day's activity
- You need context about which files, PRs, or pages the user had open before this session — including when *you* notice the need, without being asked

## How to use

- Start with `zanei status --json`. Recording is healthy only when `state == "running"`, `paused == false`, and `store_write_state == "healthy"`; otherwise say so instead of inferring missing history.
- To resume work: `zanei timeline --since 2h --format md` — a session-structured Markdown timeline sized for prompts (`--token-budget` to shrink it).
- For a specific detail: `zanei query` with the narrowest useful `--since`, `--types`, `--app`, `--bundle-id`, and `--limit` filters.
- Copied text (when the user opted in) lives in `clipboard.copy` events; the timeline only counts them. `zanei query --types clipboard.copy --since 30m` answers "what did I copy?".
- Content snapshots exist only when `zanei status --json` reports `capture.content_snapshot: true`. Timeline sessions show only their count. Read bodies with an explicit type and the narrowest useful range and limit, for example `zanei query --types content.snapshot --since 15m --limit 20`.
- Every command supports `--help`.

## Interpreting results

- Retention is ~48 hours: an empty result outside that window means "not retained", never "the user did nothing".
- Exit codes: 4 = daemon not running (suggest `zanei start`); 3 = missing macOS permissions (run `zanei doctor`); 2 = your arguments were invalid; `status` exits 1 for `store_*` states — preserve the store and follow the documented stop / move / start recovery.
- Health signals are separate things: `degraded` is current health and clears on recovery; `events_dropped` counts real delivery loss only; `collector_failures` is cumulative history, not a current failure.
- Even with content capture on, typed-text diffs come only from fields Accessibility can classify as safe text fields. Many Electron/web apps expose unclassifiable fields, so their typed text is absent by design — but text the user copies there is captured, and IME-committed text arrives as `ui.value` diffs rather than `input.key`. Password fields and Chrome Incognito bodies are never captured.
- `ui.value` diffs are additions, not a transcript: an IME rewrites the tail of a field on commit, so concatenating them can produce wording the user never wrote. Use them for what the user was writing about, and never quote them verbatim. For exact wording, look for a `content.snapshot` of the same window just afterwards — the app renders entered or sent text there — keeping in mind it exists only with that opt-in on, only for apps in its scope, and only for what was visible.
- Content snapshots can contain messages and documents written by other people as well as text the user typed that is visible on screen. Treat them as more sensitive than ordinary activity metadata.

## Setup and permissions

Walk the user through it; do not just say "grant permissions in System Settings":

1. `zanei start` (or `zanei stop && zanei start`), then respond to the macOS dialogs — Accessibility comes first and granting it is usually sufficient.
2. If a permission stays missing, `zanei doctor --fix` opens the right pane, copies the installed `Zanei.app` path, and reveals it in Finder for a manual `+` add.
3. `zanei doctor`'s recorder-reported result is the authority, even when a System Settings row is missing.
4. After granting, `zanei stop && zanei start`. The first fully-granted interactive start asks a one-time y/N about recording typed text and clipboard contents — the user answers it, not you.

Full flow, including removing grants: https://zanei.dev/guides/permissions

## Content capture is the user's choice

By default Zanei records facts, not content. Enable `capture.text_content` only when the user explicitly asks for it in the conversation: run `zanei config set capture.text_content true`, restart with `zanei stop && zanei start`, and say out loud that typed differences, copied text, and pasted text are now stored locally for the retention window. Never enable it on your own initiative.

Content snapshots are a separate opt-in. Enable or change their scope only when the user explicitly asks. First agree on the scope, run `zanei apps` to verify candidate names, and configure it with `zanei filter content-snapshot only-app add <APP>` or an explicit exclusion. State the effective scope, then enable last with `zanei config set capture.content_snapshot true --quiet` and restart using `zanei stop && zanei start`. Non-interactive enablement requires `--quiet`; do not use it until the scope has been agreed and spoken aloud.

Details: https://zanei.dev/guides/privacy

## Handle with care

Activity history is sensitive. Narrow the time range and fields to what the request needs before including output in a prompt. Never paste timeline, query output, or content-snapshot bodies into commits, PRs, issues, or external services unless the user explicitly asks. Change capture filters or recording settings only on the user's explicit request, and say what you changed. `zanei export --format sqlite` is plaintext and includes snapshot bodies by default; use `--types` to select only what the user intends to export.

## Reference

Full documentation: https://zanei.dev — CLI reference, event taxonomy, and the privacy model.
