---
name: zanei
description: Query what the user did on this computer outside the current conversation — which apps, windows, files, and web pages they used, and when (last ~48h, read-only). Reach for it whenever off-conversation context would help - resuming interrupted work, resolving vague references like "that doc I was reading" or "the error I saw", reconstructing activity before this session, or drafting standups and work logs.
---

# Zanei

Zanei records the user's activity on this machine as OS-level events — app switches, window titles, UI interactions, Chrome URLs. Everything stays in a local store and is deleted after about 48 hours by default. There are no screenshots and no keystroke content unless the user explicitly opted in.

## When to use

- The user resumes interrupted work ("continue where I left off", "what was I working on?")
- The user asks what they recently viewed, edited, or read ("which Stripe doc was I reading?")
- The user references something from outside this conversation that you cannot see — "that doc", "the error I saw", "the PR I reviewed this morning" — and you need to resolve what they mean
- Drafting a standup or work log from the day's activity
- You need context about which files, PRs, or pages the user had open before this session — including when *you* notice the need, without being asked

## How to use

- Start with `zanei status --json`. Treat recording as healthy only when `state == "running"`, `paused == false`, and `store_write_state == "healthy"`. If recording is stopped, paused, reports a `store_*` state, or has an unhealthy `store_write_state`, say so instead of inferring missing history.
- To resume work: `zanei timeline --since 2h --format md` — a session-structured Markdown timeline sized for prompts (`--token-budget` to shrink it).
- For a specific detail: `zanei query` with the narrowest useful `--since`, `--types`, `--app`, `--bundle-id`, and `--limit` filters.
- Every command supports `--help`.

## First-time setup (guiding the human)

For first-time setup, walk the user through the whole start-first flow; do not just say "grant permissions in System Settings":

1. Have them run `zanei start` first (`zanei stop && zanei start` if it is already running). The recorder asks macOS for required permissions one at a time, Accessibility first and then Input Monitoring. Exit code 3 means the recorder started but reported missing permissions; it remains running.
2. Open the right pane for them: `open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"` (Accessibility) or `?Privacy_ListenEvent` (Input Monitoring). Permission dialogs identify the bundled app as `Zanei`, and Accessibility adds its row automatically. Input Monitoring may omit the row even after the dialog grant takes effect.
3. If they want to manage an omitted Input Monitoring row from the list, have them click `+` and select the installed `Zanei.app`. While the permission is still reported missing, `zanei doctor --fix` opens the pane and copies that exact path. The bundled entry persists. A raw `cargo install` entry may not persist, and bundle-ID resets cannot address it; recommend the app bundle distribution.
4. Run `zanei stop && zanei start` so the recorder picks up the granted permissions. If no explicit choice is already saved, have the user answer the one-time text-content question themselves on the first successful interactive background start: `y` enables typed-text and clipboard-content recording; Enter accepts the default `N`. The answer is saved, and later changes use `zanei config set capture.text_content <true|false>`.
5. Run `zanei doctor`. **Its recorder-reported result is the authority**, even if the System Settings list looks different. Then confirm events are flowing with `zanei status --json`.
6. Chrome's Automation permission appears as a normal dialog the first time URL capture runs — the user just clicks Allow.

To remove Zanei's grants, the user switches an existing `Zanei` row OFF. If they explicitly want to clear a saved decision, have them stop recording and run the matching service reset: `tccutil reset Accessibility dev.zanei.recorder` or `tccutil reset ListenEvent dev.zanei.recorder`. These bundle-ID resets do not apply to a raw unbundled binary. See https://zanei.dev/guides/permissions for the complete flow.

## Interpreting results

- Retention is ~48 hours: an empty result outside that window means "not retained", never "the user did nothing".
- Exit code 4 = recording daemon is not running (suggest `zanei start`). Exit code 3 = missing macOS permissions; from `start`, it means the daemon remains running but its heartbeat reported missing permissions, while from `doctor` it is the diagnostic result. 2 = your arguments were invalid. `status` exits 1 for `store_missing`, `store_unavailable`, or `store_corrupt`; preserve the store and follow the documented stop / move / start recovery procedure.
- Evaluate sparse history signals separately: `degraded` is current health and clears on recovery; `events_dropped` counts actual delivery loss; `collector_failures` is cumulative history and does not by itself indicate a current failure. An AX observer warning covers only apps used since daemon startup: apps activated after startup or currently frontmost. It clears when observer attachment succeeds.
- `events_dropped` is the cumulative count of events intended for recording but actually lost to backpressure, a full queue, disconnection, or a similar delivery failure. It excludes inputs outside the recording scope because they cannot be attributed to an app or window, and unflushed data lost to a crash or `SIGKILL`.

## Richer capture (opt-in)

By default Zanei records facts, not content: typing events carry no text, clipboard events carry no bodies, and UI events carry no input differences. If no explicit choice is already saved, the first successful interactive background `start` asks the user to choose; the default answer is `N`, the answer is saved, and later changes use `zanei config set capture.text_content <true|false>`. If the user wonders why content is missing, tell them about this `capture.text_content` opt-in and its trade-off: authorized direct non-IME keystrokes, newly added input differences, and clipboard contents can be stored locally for the retention window, including sensitive text outside secure fields. Copy text requires a matching Command-C from the same process; input and paste text require a known non-secure Accessibility field. A confirmed keystroke or paste opens a non-consuming 3-second window for the same app and focused-element generation, and can authorize multiple `ui.value` notifications; rejected inputs open no window. `ui.value` changes are batched after 1 second without a new observation or at most 5 seconds after the first pending observation, whichever comes first. Chrome Incognito and website-filtered windows keep text-content bodies null, although titles and interaction metadata can remain. While an IME is active, `input.key` carries no text and committed input may appear only as a `ui.value` difference. Voice input does not open an authorization window, but voice-inserted text can be included in a value difference if it arrives while a window opened by a preceding keystroke or paste is still active.

When — and only when — the user explicitly asks for content capture in the conversation, you may enable it for them: run `zanei config set capture.text_content true`, restart recording with `zanei stop && zanei start`, and confirm out loud that content is now being recorded. Never enable it on your own initiative, and never because it would merely be useful.

## Handle with care

Activity history is sensitive. Narrow the time range and fields to what the request needs before including output in a prompt. Never paste timeline or query output into commits, PRs, issues, or external services unless the user explicitly asks. Change capture filters or recording settings only on the user's explicit request in the conversation, and say what you changed.

## Reference

Full documentation: https://zanei.dev — CLI reference, event taxonomy, and the privacy model.
