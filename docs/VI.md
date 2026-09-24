# Remuda Visual Identity

## Design rationale

Two parallel tracks turn in the same direction to form an abstract lowercase `r`. The tracks represent multiple accounts; their shared direction represents session continuity. The symbol is provider-independent and remains recognizable in a single color.

Use simple geometry, neutral backgrounds, and small lime accents. Write **Remuda** in prose and **remuda** in the wordmark and commands.

## Logo

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../assets/brand/lockup-reverse.svg">
  <img src="../assets/brand/lockup.svg" alt="Remuda horizontal logo" width="280">
</picture>

- The symbol uses a 128 × 128 canvas, with visible bounds from `(8, 8)` to `(120, 120)`. Track width is 20 units; the gap between straight sections is 12 units.
- Leave at least one track width of clear space around the visible symbol. Add padding where the SVG's built-in margin is insufficient.
- Keep the horizontal logo at least 120px wide. Use the dedicated favicon at 16–24px and the app icon at larger sizes.
- Use Ink on light backgrounds and the reversed Paper version on dark backgrounds. Do not stretch, rotate, add shadows, or change the track spacing.
- SVGs are editable masters. Wordmark letters are converted to paths, so no font installation is required.

## Colors

| Color | sRGB | Use |
| --- | --- | --- |
| Ink | `#20251F` | Primary text, logos, dark backgrounds |
| Paper | `#F4F2E9` | Light backgrounds, reversed logos |
| Lime | `#D5F36B` | Occasional accents, selected items |
| Moss | `#64715B` | Secondary text on Paper |

Use Ink text on Lime, never white. Contrast ratios are 13.91:1 for Ink/Paper, 12.53:1 for Ink/Lime, and 4.62:1 for Moss/Paper. Lime is a brand color, not an indicator of success, warnings, or remaining capacity.

## Typography

The wordmark uses **Manrope at weight 750** with adjusted letter spacing. Use Manrope for Latin display text and IBM Plex Mono for code and data. Use system sans-serif fonts for Chinese text and respect the user's font choice in terminals.

Manrope is licensed under SIL OFL 1.1; its [copyright and license notice](../assets/brand/OFL-Manrope.txt) is retained. For font editing, obtain [Manrope](https://github.com/google/fonts/tree/main/ofl/manrope) or [IBM Plex Mono](https://github.com/google/fonts/tree/main/ofl/ibmplexmono) from their source directories. Font files are not bundled with these assets.

## Assets

All files are in [`assets/brand/`](../assets/brand/).

| Asset | Files |
| --- | --- |
| Horizontal logo | [Ink SVG](../assets/brand/lockup.svg) · [Paper SVG](../assets/brand/lockup-reverse.svg) |
| Standalone symbol | [Ink SVG](../assets/brand/mark.svg) · [Paper SVG](../assets/brand/mark-reverse.svg) · [Transparent PNG, 512px](../assets/brand/mark-512.png) |
| App icon | [SVG](../assets/brand/icon.svg) · [PNG, 512px](../assets/brand/icon-512.png) |
| Favicon | [SVG](../assets/brand/favicon.svg) · [PNG, 32px](../assets/brand/favicon-32.png) |
| Supporting graphic | [Parallel tracks SVG](../assets/brand/tracks.svg); may be cropped or extended, but must not replace the logo |

Colors are specified in sRGB for screens; printed materials require separate proofing. No trademark similarity search has been performed.
