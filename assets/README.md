# soloc brand assets

The mark is an instrument. The sun at the centre is the root frame; the **orbit is the ruler**,
a graduated open track with a small circle marking where it starts; and two bodies stand on the
two orbits as the hour and minute of a single instant. Both rings are inclined — foreshortened
0.86 and rolled −12° — so they read as rings seen at an angle rather than a flat dial.

The instant is **02:56 UTC, 21 July 1969** — Armstrong stepping onto the ladder.

## Files

| File | Use |
|---|---|
| `soloc-icon.svg` | Primary app icon, 400×400 with a 22% corner radius. |
| `soloc-icon-square.svg` | Same artwork, square. Use where the host applies its own mask. |
| `soloc-icon-compact.svg` | Heavier build for 48px and below: no inner orbit, both bodies on the graduated track, four graduations. |
| `soloc-icon-mono.svg` | Single colour (white on night). Embroidery, stamps, print. |
| `soloc-lockup.svg` | Icon + wordmark for light backgrounds. Text is outlined — no font needed. |
| `soloc-lockup-dark.svg` | Same lockup for dark backgrounds. |
| `soloc-icon-512.png` … `-64.png` | Rasters of the primary icon. |
| `favicon-32.png`, `favicon-16.png`, `favicon.ico` | Compact build. |

## Palette

| Role | Hex |
|---|---|
| Night (field) | `#151a23` |
| Sol (sun, graduated track, start circle) | `#e9af35` |
| Inner track | `#5f7a99` |
| Bodies | `#ccd4dd` |
| Stars | `#ffffff` |

Amber is the instrument — the sun, the graduated track and its start circle. Cool is what the
instrument observes — the inner orbit and the two bodies. Do not add a second warm colour.

## Geometry

- Graduated track r=140, stroke 8. Inner track r=82, stroke 4.5. Sun r=34.
- Inclination: 0.86 foreshortening, −12° roll, applied to both tracks.
- Graduations every 30° of the dial, 14 units long, 24 at the cardinals. They point at the
  centre and are held to one length on screen, so the inclination never makes them ragged.
- The gap spans 52° and sits in the larger empty arc between the two bodies, never over
  12 o'clock. The start circle sits at the leading end of the gap.

## Wordmark

Space Grotesk Bold (700), tracking −0.035em, all lowercase. The lockup SVGs carry the wordmark
as outlines, so they render correctly without the font installed. For live text, load Space
Grotesk and fall back to a neutral grotesque.

## Rules

- Never recolour the amber to a second accent, and never put a gradient on anything.
- Keep clear space of at least the sun's diameter around the lockup.
- Below 48px use the compact build.
- The track's gap and its start circle are load-bearing. Do not close the track.
