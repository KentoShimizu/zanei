---
name: zanei
description: Query what the user did on this computer outside the current conversation — which apps, windows, files, and web pages they used, and when (last ~48h, read-only). Reach for it whenever off-conversation context would help - resuming interrupted work, resolving vague references like "that doc I was reading" or "the error I saw", reconstructing activity before this session, or drafting standups and work logs.
---

# Zanei

Zanei records the user's activity on this machine as OS-level events — app switches, window titles, UI interactions, Chrome URLs. Everything stays in a local store and is deleted after about 48 hours by default. There are no screenshots, and no typed or copied content unless the user explicitly opted in.

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
- Every command supports `--help`.

## Interpreting results

- Retention is ~48 hours: an empty result outside that window means "not retained", never "the user did nothing".
- Exit codes: 4 = daemon not running (suggest `zanei start`); 3 = missing macOS permissions (run `zanei doctor`); 2 = your arguments were invalid; `status` exits 1 for `store_*` states — preserve the store and follow the documented stop / move / start recovery.
- Health signals are separate things: `degraded` is current health and clears on recovery; `events_dropped` counts real delivery loss only; `collector_failures` is cumulative history, not a current failure.
- Even with content capture on, typed-text diffs come only from fields Accessibility can classify as safe text fields. Many Electron/web apps expose unclassifiable fields, so their typed text is absent by design — but text the user copies there is captured, and IME-committed text arrives as `ui.value` diffs rather than `input.key`. Password fields and Chrome Incognito bodies are never captured.

## Setup and permissions

Walk the user through it; do not just say "grant permissions in System Settings":

1. `zanei start` (or `zanei stop && zanei start`), then respond to the macOS dialogs — Accessibility comes first and granting it is usually sufficient.
2. If a permission stays missing, `zanei doctor --fix` opens the right pane, copies the installed `Zanei.app` path, and reveals it in Finder for a manual `+` add.
3. `zanei doctor`'s recorder-reported result is the authority, even when a System Settings row is missing.
4. After granting, `zanei stop && zanei start`. The first fully-granted interactive start asks a one-time y/N about recording typed text and clipboard contents — the user answers it, not you.

Full flow, including removing grants: https://zanei.dev/guides/permissions

## Content capture is the user's choice

By default Zanei records facts, not content. Enable content capture only when the user explicitly asks for it in the conversation: `zanei config set capture.text_content true`, restart with `zanei stop && zanei start`, and say out loud that content is now recorded. Never enable it on your own initiative. The trade-off to convey: typed differences, copied text, and pasted text are then stored locally for the retention window, which can include sensitive text outside password fields. Details: https://zanei.dev/guides/privacy

## Handle with care

Activity history is sensitive. Narrow the time range and fields to what the request needs before including output in a prompt. Never paste timeline or query output into commits, PRs, issues, or external services unless the user explicitly asks. Change capture filters or recording settings only on the user's explicit request, and say what you changed.

## Reference

Full documentation: https://zanei.dev — CLI reference, event taxonomy, and the privacy model.
