<div align="center">
  <img src="docs/public/favicon.svg" width="96" alt="Zanei icon">
  <h1>Zanei</h1>
  <p><strong>Local activity context for AI agents</strong></p>
  <p>
    <a href="https://zanei.dev">Documentation</a>
    (<a href="https://zanei.dev/ja/">日本語</a>)
  </p>
  <p>
    <img src="https://github.com/KentoShimizu/zanei/actions/workflows/ci.yml/badge.svg" alt="CI">
    <img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-1e1f22" alt="License: MIT OR Apache-2.0">
    <img src="https://img.shields.io/badge/platform-macOS-1e1f22" alt="Platform: macOS">
  </p>
</div>

Zanei (残映, *zan-ei*) records your activity on macOS as OS-level events rather than screenshots or screen
video, keeps those events in a local SQLite store, and turns them into session-structured,
deduplicated timelines that AI agents can read through the CLI or read-only MCP server.

> **Platform support: macOS only, for now.** Zanei currently runs on macOS (Apple Silicon and
> Intel). Windows and Linux are on the roadmap.

## Privacy by default

- Zanei has no network egress path; recorded data stays in the local SQLite store.
- Typed and field content is captured only when `capture.text_content = true`; the default is off.
- Secure text fields are excluded at capture time. Chrome Incognito suppresses URL events and text-content bodies; titles and interaction metadata can remain.
- Events are deleted automatically after 48 hours by default.

See the [privacy model](https://zanei.dev/guides/privacy) for the full guarantees and
limitations.

## Installation

```bash
brew install kentoshimizu/tap/zanei
```

Signed and notarized. To build from source instead, see the
[packaging instructions](packaging/README.md) — note that a raw `cargo install`
binary is not the distributed app bundle and its macOS permission rows may not
persist; use the app bundle for stable permission management.

## Quickstart

Start the recorder so macOS can ask for the required permissions, grant them in System Settings,
restart the recorder, then verify and retrieve a recent timeline:

```bash
zanei start
```

On first setup, `start` may exit with code 3 after the recorder starts and reports missing
permissions. The permission dialogs identify the bundled app as `Zanei`, and Accessibility adds
its row automatically. Input Monitoring may omit the row even after the dialog grant takes effect;
to manage that permission from the list, use `+` and select the installed `Zanei.app`. While the
permission is still reported missing, `zanei doctor --fix` opens the pane and copies that exact
path. Then restart and trust the recorder-reported `doctor` result before reading the timeline:

```bash
zanei stop && zanei start
zanei doctor
zanei timeline --since 15m
```

After the restart, `doctor` verifies the permission state enforced for the recorder itself. You can
remove access later from an existing `Zanei` row or with the service-specific reset commands in the
[permissions guide](https://zanei.dev/guides/permissions#remove-or-reset-permissions).

## Agent integration

Install the agent skill and print the MCP registration command with `setup`:

```bash
zanei setup --agent claude
```

See [agent setup](https://zanei.dev/agents/setup) for Codex, opencode, Hermes Agent, pi,
Claude Desktop, scope selection, and dry-run instructions.

## License

Zanei is licensed under either the [MIT License](LICENSE-MIT) or the
[Apache License 2.0](LICENSE-APACHE), at your option.
