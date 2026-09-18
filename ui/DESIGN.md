# Caudal UI — design system

**Material Design 3** (m3.material.io) is the design system. Decided 18 Sep 2026.

## Why tokens, not Google's component library
`@material/web` (Google's M3 web components, Apache-2.0, v2.5.0) is **"in maintenance mode pending new maintainers"** per its own README. We follow the M3 spec and use Google's color engine, but build components in React + Tailwind to that spec. No dependency on a paused library.

## Color
Brand palette (from `~/Downloads/colors for app.jpg`):

| Name | Hex | M3 role source |
|---|---|---|
| Orange | `#F54F1B` | **primary**: action, LIVE, focus, the one thing to press |
| Space Cadet | `#1E223D` | **secondary** + neutral hue: structure, dark surfaces |
| Gargoyle Gas | `#E6D5B7` | **tertiary**: warm accents, highlights, light containers |

Tokens are generated, never hand-edited: `ui/theme/generate.mjs` feeds the three colors to Google's `@material/material-color-utilities` 0.4.0 and writes `ui/theme/tokens.css` + `tokens.json` (every M3 color role, light and dark, as `--md-sys-color-*`). Primary, secondary and neutral roles come from the **Fidelity** variant (keeps the orange true: dark primary container `#ff5723`); tertiary roles from **Tonal Spot** (keeps Gargoyle Gas true: light tertiary container `#f2e0c2`); neutrals use Space Cadet's hue at chroma 14 so dark surfaces are blue (`#101221` / `#1d1f2e`), not gray. Every text/background pair passes WCAG AA in both themes; the generator prints the report.

Regenerate: `cd ui/theme && npm install && npm run build`.

**Dark is the default.** Streaming ops happen in control rooms and at night; dark also makes video look right. Light follows `prefers-color-scheme` and a manual toggle (`data-theme`).

## The rest of M3
| Token set | Choice |
|---|---|
| Typography | M3 type scale (display / headline / title / body / label). **Roboto Flex** for UI (headlines condensed: wdth 62, weight 750), **Roboto Mono** for every number that updates live (bitrate, viewers, timestamps), so digits don't jitter. |
| Shape | M3 shape scale: extra-small 4, small 8, medium 12, large 16, extra-large 28 px; full for chips and the LIVE badge. |
| Elevation | Tonal: `surface-container-*` levels, shadows only on floating elements (menus, dialogs, FAB). |
| State layers | M3 opacities: hover 8%, focus 10%, pressed 10%, dragged 16%, on the content color. |
| Motion | M3 easing and duration tokens (`emphasized`, `standard`; short 100-200 ms, medium 250-400 ms); `prefers-reduced-motion` respected. |
| Density | M3 density -2 for tables of streams and connections. |

## Semantic use of color (so the palette carries meaning)
- **Orange = live and action.** LIVE badge, primary buttons, focus ring, the playhead. Nothing decorative is orange.
- **Space Cadet = structure.** Navigation rail, surfaces, secondary buttons.
- **Gargoyle Gas = warm information.** Selected rows, info banners, highlighted stats.
- **Error role** (M3 generated red) for faults only; stream health uses icon + text, never color alone.

## Screens (parity with MistServer's UI, redesigned)
Navigation rail: Overview · Streams · Ingest (protocols) · Push · Triggers · Logs · Stats · Keys · Settings. Stream detail: player (hls.js / WHEP / MoQ), tracks, viewers, health, embed code.
