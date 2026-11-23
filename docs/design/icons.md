# App and tray icons

Run `bun run icons` to regenerate the assets. The mirrorball geometry lives in
`scripts/mirrorball.mjs`; `scripts/icons.mjs` renders the app and tray icons.
Edit these scripts rather than the generated files.

| Asset                                             | Use                               |
| ------------------------------------------------- | --------------------------------- |
| `public/icon.png`                                 | App icon and favicon              |
| `build/icon.icns`                                 | macOS app icon (packaging only)   |
| `public/tray/ball.svg`, `public/tray/stopped.svg` | Interface logo                    |
| `public/tray/mac-{ball,stopped}[@2x].png`         | macOS menu bar, 18pt at 1× and 2× |
| `public/tray/linux-{ball,stopped}.png`            | Linux tray, 22px                  |
| `public/mirrorball-spin.png`                      | Finite logo animation sprite      |

The tray is static: colored when at least one profile is running, gray when none
are running. The tooltip reports active profile names and whether requests are
streaming. The interface logo animates briefly on opening or clicking and respects
reduced motion.

Generated assets are committed so normal builds do not need to regenerate them.
The daemon compiles the tray PNGs into its binary, so rebuild it after regenerating them.
The icon command also regenerates the OpenCode provider symbol from Lucide.
