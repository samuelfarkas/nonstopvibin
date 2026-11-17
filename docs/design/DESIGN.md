# nonstopvibin visual system

Register: product UI (a local workbench for pooled coding subscriptions). The
tool should disappear into the task. Density is welcome; decoration is not.

## Scene

A developer with several paid coding subscriptions, terminal open beside this
window, glancing at headroom before starting a long agent session. Dark,
warm-charcoal surfaces; one amber accent; Geist (bundled via @fontsource-variable)
for navigation, labels, prose, and account names. Geist Mono is reserved for
numeric data, code, and paths. Only the NonstopVibin wordmark uses Space Grotesk,
also bundled via @fontsource-variable.
Readability at actual size on a large monitor wins over matching tiny mockup text.

## Tokens (src/client/styles.css)

- Surfaces: `--bg-0` canvas, `--bg-1` main, `--bg-2` sidebar/panels/modals,
  `--bg-3` hover/selected, `--bg-4` strongest fill. Borders `--line`,
  `--line-strong`.
- Text: `--ink` primary, `--ink-2` secondary, `--ink-3` muted (still ≥ 4.5:1
  on `--bg-1`), `--ink-4` disabled/decorative only.
- Accent: `--amber` (primary buttons, selected marker, low headroom),
  `--amber-text` for amber text, `--amber-dim` tint. `--green` running/ok,
  `--red` errors. Never use amber or green decoratively.
- Meters: `--fill` neutral fill, `--track` track, `.meter-track.low` amber,
  `.meter-track.empty` red. Tracks are 0.25rem.
- Type: `--text-caption` 0.875rem, `--text-body` 1rem,
  `--text-subheading` 1.25rem, `--text-heading` 1.5rem,
  `--text-metric` 2rem. Normal base is the browser default (16px); step to
  112.5% at 1920 CSS pixels and 125% at 2560. No device-pixel/DPI guessing.
  Main-window labels and metadata are at least 14px; account values are 20px.
  Native View > Zoom is available for individual display preferences.
- Tray exception: the fixed-width menu-bar panel (`trayWidth` in
  `src/desktop/main.ts`) uses 14px names and 12px
  metadata. It does not inherit the main window's large-display scale.
- Radii: `--radius` 6px controls, `--radius-lg` 10px panels/modals. Nothing
  rounder.
- Motion: 150–250ms, `--ease` (ease-out-expo). State changes only. Reduced
  motion is handled globally.
- z-index scale: `--z-sticky`, `--z-overlay`, `--z-modal`, `--z-toast`.

## Vocabulary (reuse, don't re-invent)

- `.label` — sans, caption size, semibold, sentence case. Column headers,
  field captions, and section captions; no tiny tracked uppercase labels.
- `.button`, `.button.primary`, `.button.small`, `.button.danger`,
  `.text-button`, `.icon-button`, `.segmented > button.active`.
- `.status` + `.status-dot` (`.good`/`.bad`), `.tag` (`.live`/`.warn`/`.out`/
  `.bad`) — short sentence-case state labels, no backgrounds.
- `.data-table` (`th.num`/`td.num` right-aligned) — mono caption size, 1px row lines,
  no zebra, no card wrapper.
- `.panel` — bordered `--bg-2` box. Use sparingly; one panel level, never
  nested panels.
- `.quota-meter` / `QuotaMeter` — label, value, track, reset line.
- `.field`, `.field-note`, `.inline-error`, `.info-note`, `.modal-footer`,
  `.form-grid`, `<details>/<summary>`.
- `.empty-state` — left-aligned, amber icon, h2, prose, optional `.empty-choices`
  list. Keep this separate from the `.empty` modifier for exhausted quotas.
  `.loading` — mono inline loading row.
- Page structure: `.page-header` (sticky, title + mono subtitle + actions),
  `.page-body` (2rem gutters; 1.125rem in small windows), `.page-footer`.
- Shell: always fills the window; never cap `.app` or `.main`. Data uses up to
  90rem (110rem at 2560 CSS pixels), centered with aligned header/footer gutters.
  Prose remains bounded to 65–75ch. The sidebar stays at the window edge.
  Account identity is capped at 24rem; extra room goes to readable quota labels.
  Use 1rem vertical row padding. Identity and limits stay beside each other
  above 900px; smaller windows reflow without shrinking text. Email and plan
  get separate wrapping lines, not one ellipsized metadata string.

## Rules

- Product register: familiar affordances, every control has hover/focus/
  disabled states, skeleton or inline loading, empty states that teach.
- No side-stripe accents wider than 2px, no gradient text, no glass, no
  identical card grids, no big-number-with-gradient hero cards, no numbered
  section markers unless the section is a real sequence (setup steps are).
- Numeric data is mono + `tabular-nums`. Use sans for reset descriptions and
  account metadata; preserve mono for technical identifiers and code.
- Copy is short and specific. No marketing lines inside the app.
- Reference mockups for the target feel live with the task brief; match the
  feel, not pixel geometry. Real data shapes win over invented metrics.
