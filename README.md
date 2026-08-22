<div align="center">
  <img src="https://raw.githubusercontent.com/KentoShimizu/zanei/main/docs/public/favicon.svg" width="96" alt="Zanei icon">
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

Zanei (残映, *zan-ei*) records your computer activity as OS-level events — not screenshots —
into a local SQLite store and turns it into session-structured, deduplicated timelines that
AI agents read through the CLI or an MCP server. macOS only (Apple Silicon and Intel);
Windows and Linux are planned.

## Privacy by default

- No network egress path: recorded data never leaves the local store.
- Typed and clipboard content is recorded only if you opt in; the first fully-granted
  `start` asks once, and the default is no.
- Password fields are dropped at capture time. Chrome Incognito produces no URL events
  and no text bodies (window titles can remain).
- Events are deleted after 48 hours by default.
- Encrypted at rest: the store is a SQLCipher database; the key lives in your login Keychain.

Full guarantees and limits: [privacy model](https://zanei.dev/guides/privacy).

## Install

```bash
brew install kentoshimizu/tap/zanei
```

Signed and notarized. Building from source: [packaging](packaging/README.md).

## Quickstart

```bash
zanei start
```

Grant the permissions macOS asks for. If a System Settings row is missing, `zanei doctor --fix`
opens the right pane with the `Zanei.app` path copied and revealed in Finder. Then:

```bash
zanei stop && zanei start   # asks once whether to record typed text; default no
zanei timeline --since 15m
```

`zanei doctor` reports the permission state of the recorder itself, and how to revoke it.

## Agent integration

```bash
zanei setup --agent claude
```

Installs the skill and prints the MCP registration command. See
[agent setup](https://zanei.dev/agents/setup) for Codex, opencode, Hermes Agent, pi, and
Claude Desktop.

## License

Licensed under either the [MIT License](LICENSE-MIT) or the
[Apache License 2.0](LICENSE-APACHE), at your option. Third-party components compiled into
the binary and their licenses are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
