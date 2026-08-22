# Security Policy

Zanei records on-device activity, so we treat privacy boundary violations with the same
severity as classic security vulnerabilities. That includes:

- data captured from an excluded app, site, secure text field, or private browsing window
- content captured while `capture.text_content = false`
- any network transmission of recorded data (the design has no egress path at all)
- the MCP server exposing write access or data outside the documented read-only tools
- the store key being written anywhere other than the login Keychain (outside the documented
  `ZANEI_STORE_KEY_FILE` development override), or a plaintext copy of the store being written
  without an explicit `export`

## Reporting a vulnerability

Please **do not open a public issue** for security or privacy problems. Use
[GitHub private vulnerability reporting](https://github.com/KentoShimizu/zanei/security/advisories/new)
instead. You should get an initial response within a few days.

## Supported versions

Only the latest released 0.x version receives fixes.
