# Changelog

## [Unreleased]

### Fixed

- Upgraded bundled CLIProxyAPI from 7.2.151 to 8.0.8 with reviewed archive and binary
  hashes for all four targets. Pi native tool changes now retain their beta headers
  and matching tool aliases; use `/reload` after app startup updates Pi extensions.
- Errored profiles can now be restarted directly from the sidebar.
- API account and credential updates now roll back together when a write fails.
- Malformed sign-in callback URLs now receive a safe error response without stopping the app.

### Security

- CSV exports now neutralize whitespace-prefixed formulas, and release signing secrets are limited to the macOS signing steps.
- Core archives and extracted binaries now use reviewed source pins before packaging.
- Packaged resources now exclude unapproved license files and reject secret-like paths.
- Custom agent configuration roots now reject shared write access.
- Invalid quota responses no longer persist provider response excerpts in error text.

### Changed

- Installed agent connections now update automatically on app startup, including
  stopped profiles and every connected Claude project. Existing user settings are
  preserved; edited or removed integrations require manual repair.
- Desktop and development now share `~/.nonstopvibin` and an owner-only AES-GCM
  vault key. Linux no longer requires a secret service. Older data directories
  are not imported automatically; see [storage migration](docs/architecture.md).
- Added optional activity retention, full-period usage totals and estimated API
  value, plus manually confirmed Codex banked resets.
- Main and quota windows now unload when closed, preserving unfinished forms;
  the menu bar icon stays static and logo animations finish instead of looping.
- Startup failures are now also printed to the terminal.
- The first release exposes Claude Code, Codex, and pi as agents, and Claude, Codex, OpenCode Go, and custom API-key providers. Kimi, Antigravity, xAI, and the OpenCode agent stay in the code but are hidden from the app.
- Packaged Electron builds now disable run-as-Node and `NODE_OPTIONS` environment overrides.

## [0.1.1] - 2026-09-07

- Added separate profiles and account pools with round-robin and fill-first routing.
- Added native connections for Claude Code, Codex, and pi.
- Added quota windows and reset times in the main app and menu bar popup.
- Added activity history with observed token usage and CSV export.
- Added encrypted desktop storage through Electron safeStorage. CLIProxyAPI's
  required plaintext files remain restricted to the current OS user.
- Added macOS DMG and Linux AppImage/deb packaging. macOS builds are unsigned and
  not notarized.

[Unreleased]: https://github.com/samuelfarkas/nonstopvibin/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/samuelfarkas/nonstopvibin/releases/tag/v0.1.1
