# Shiv — brand assets

## SVG (`svg/`)

| File | Use |
|---|---|
| `shiv-mark.svg` | Square mark (A) for dark backgrounds. Bands are cut out, so it sits on any dark ground. |
| `shiv-mark-light.svg` | Square mark for light backgrounds (ink blade, deeper brass grip). |
| `shiv-mark-mono.svg` | Single colour, uses `currentColor`. For one-colour print, tray icons, embossing. |
| `shiv-icon.svg` | App icon tile, 512 × 512, full detail. Source for `.ico` / `.icns` / PNG sizes ≥ 64 px. |
| `shiv-icon-small.svg` | Simplified tile (no bevel, no tape bands) for 48 px and below. |
| `favicon.svg` | 32 × 32 simplified tile for the site / docs. |
| `shiv-wordmark.svg` | Horizontal wordmark (C) for dark backgrounds. Letters are outlined, no font needed. |
| `shiv-wordmark-light.svg` | Wordmark for light backgrounds. |
| `shiv-wordmark-mono.svg` | Single-colour wordmark, `currentColor`. |

Mono files inherit colour: inline them, or set `color` on the parent.

## Styles

- `css/tokens.css` — palette + semantic tokens (dark default, `data-theme="light"` on `<html>` for light), type, space, radius, motion.
- `css/base.css` — element defaults plus `.btn`, `.input`, `.card`, `.badge`, `.toggle`. Load after `tokens.css`.
- `tokens.json` — the same values as plain data, for a non-web Rust UI (egui, iced, Slint).

## Palette

| Token | Hex |
|---|---|
| Ink (ground) | `#121110` |
| Surface | `#1E1B17` |
| Bone (text, blade) | `#EDE6D6` |
| Brass (accent, grip) | `#C9A44C` |
| Brass on light | `#9C7B2E` (marks) / `#7A6128` (text) |

## Clear space and minimum size

- Keep clear space equal to the grip's width around the mark, and half the wordmark's cap height around the wordmark.
- Below 48 px use `shiv-icon-small.svg`. The wordmark should not go below 16 px tall.

Wordmark letters are outlined from Chakra Petch Bold (SIL Open Font License).
