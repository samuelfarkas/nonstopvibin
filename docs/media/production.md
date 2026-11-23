# Screenshot capture notes

The README screenshots show the real React interface and the TypeScript server running
with isolated sample data from [scripts/preview.ts](https://github.com/samuelfarkas/nonstopvibin/blob/caeab18/scripts/preview.ts),
which left with the TypeScript server.
No real credentials or provider accounts were used.

- `subscriptions.png`: account pools, remaining quotas, and reset times.
- `activity.png`: seven days of sample request history and token usage.
- `connect.png`: Codex configuration completed in an isolated agent home, with
  the launch command displayed. Codex was not launched.

Captured September 14, 2026 at 2880 × 1620 from source
`bbc1e0ca4e82f083795b2cd110a8033ef1e204df`. The source build passed before capture.
An ignored preview copy used ports 4521/4522 and distinct sample model IDs for
Claude and Codex to demonstrate provider selection.

These are browser preview captures, not captures of the app window. Live
inference, quota retrieval, reset redemption, and menu-bar behavior are outside
their scope.

The Rust daemon has no sample-data simulator, so `bun run preview` no longer
exists. To refresh the captures, run the app with a separate
`NONSTOPVIBIN_DATA_DIR` and synthetic accounts, open the session link in an
isolated browser, and capture each view at a 1440 × 810 viewport with 2× scale.
