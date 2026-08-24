# Packaging

Zanei ships as a background-only macOS app bundle. `Zanei.app/Contents/MacOS/zanei` is the existing CLI binary, and installers expose `bin/zanei` as a symlink to that executable. The bundle gives macOS one stable TCC identity: `dev.zanei.recorder`.

Release artifacts use Developer ID signing, notarization, and stapling. `make-app.sh` performs the same bundle assembly and signing locally; it does not notarize.

## LaunchAgent registration

Run `zanei start` to start background recording. The CLI generates the current LaunchAgent plist at `$HOME/Library/LaunchAgents/dev.zanei.agent.plist` and registers it with `launchd`. The generated plist uses the resolved Zanei executable together with the active config, store, and log paths. The CLI implementation is the canonical source for launchd settings; packaging does not provide a separate plist template.

## Homebrew formula inputs

The Homebrew formula is published from the separate tap repository after replacing `@VERSION@` and `@SHA256@` with the universal release values. It installs `Zanei.app` under `libexec` and creates only the CLI symlink in `bin`.

## Build a local app bundle

Build the release binary, then pass a code-signing identity to the bundle builder:

```bash
cargo build --release -p zanei-cli
./packaging/make-app.sh "Zanei Local Development"
/usr/bin/codesign --verify --strict --verbose=2 dist/Zanei.app
/usr/bin/codesign --display --verbose=4 --entitlements - dist/Zanei.app
```

Use `-` as the identity for an ad-hoc local bundle when permission continuity across rebuilds is not required. A persistent local development certificate keeps the signing identity stable:

```bash
./packaging/make-app.sh -
mkdir -p "$HOME/.local/libexec/zanei" "$HOME/.cargo/bin"
ditto dist/Zanei.app "$HOME/.local/libexec/zanei/Zanei.app"
ln -sfn "$HOME/.local/libexec/zanei/Zanei.app/Contents/MacOS/zanei" \
  "$HOME/.cargo/bin/zanei"
```

`make-app.sh` reads the version reported by the compiled binary, writes it to both bundle version keys, signs the bundle with Hardened Runtime and `entitlements.plist`, and verifies the result. Its optional second and third positional arguments override the input binary and output app paths for release automation and tests. `--timestamp` enables the secure timestamp used by the release workflow.

An unbundled `cargo install` binary does not have the app bundle's TCC identity. It can retain the older behavior where permission rows are omitted and `tccutil reset ... dev.zanei.recorder` cannot address it. Never edit the TCC database or attempt to grant permissions programmatically.
