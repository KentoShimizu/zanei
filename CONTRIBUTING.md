# Contributing to Zanei

Thanks for your interest in Zanei. The project is in an early, fast-moving stage, so
contributions currently work as follows.

## Issues first — no pull requests yet

**We are not accepting pull requests for now.** The architecture and contracts are still
settling, and unsolicited PRs are likely to be closed. Instead:

- **Bug reports and feature requests are very welcome** — please use the issue templates.
- If you want to work on something, open an issue and discuss it first. Once the project
  stabilizes, this policy will be relaxed.

## Reporting bugs

Please include:

- `zanei --version`, your macOS version, and whether you built from source
- `zanei doctor --json` output when the problem involves recording or permissions
- Steps to reproduce

**Privacy note:** never paste raw store contents, timelines, or event dumps into a public
issue — they contain your activity history. Redact aggressively; a single synthetic event
that reproduces the shape of the problem is enough.

Security and privacy vulnerabilities go through [SECURITY.md](SECURITY.md), not public issues.

## Development setup

```bash
rustup toolchain install stable   # pinned by rust-toolchain.toml
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```

Builds from source carry an ad-hoc code signature that changes on every build, so reading the
store key from the login Keychain would trigger a Keychain dialog after each build. Set
`ZANEI_STORE_KEY_FILE` so every Zanei process reads the key from a file instead; the recorder
creates the file (mode 0600) if it is missing:

```bash
export ZANEI_STORE_KEY_FILE=~/.config/zanei/dev.key
```

Tests set it automatically. The override is for development only: the key sits on disk.

The documentation site lives in `docs/` (pnpm + Blume): `pnpm --dir docs dev`.

Where things are defined:

- **Interface contracts** (CLI, MCP, events): the documentation site under `docs/content/reference/`
  and the JSON Schema at `docs/public/schema/event.schema.json`
- **Crate boundaries**: `zanei-core` (OS-independent schema, store, privacy, timeline) ←
  `zanei-collector` (capture contract) ← `zanei-macos` (macOS collectors) ← `zanei-cli` (the binary);
  `zanei-mcp` depends only on the read side of `zanei-core`

## License

Zanei is dual-licensed under MIT or Apache-2.0. Unless you explicitly state otherwise,
any contribution intentionally submitted for inclusion in the work by you shall be dual
licensed as above, without any additional terms or conditions.

Third-party components compiled into the binary, such as SQLCipher (BSD-3-Clause), are listed
in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
