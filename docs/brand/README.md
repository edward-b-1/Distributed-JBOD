# Distributed JBOD — logo

## The mark

Four columns of pill-shaped slabs. Each column is a **node**; each slab is a chunk of
**disk capacity**, and the columns are deliberately different heights (4 / 6 / 3 / 5) —
that is the "mixed-size devices" part. One slab per column is **orange**, and those
orange slabs step diagonally across the whole array: the **parity / checksum stripe**
that rotates across nodes, which is the bitrot-resilience story.

So: heterogeneous capacity, pooled across nodes, with integrity data striped over all of it.

## Files

| File | Use |
|---|---|
| `djbod-lockup-horizontal.svg` | primary lockup, name set on two lines — README headers, docs, site nav |
| `djbod-lockup-horizontal-onDark.svg` | same, for dark backgrounds |
| `djbod-lockup-inline.svg` / `-onDark.svg` | one-line lockup, `Distributed-JBOD` — matches the repo name |
| `djbod-lockup-inline-nohyphen.svg` / `-onDark.svg` | one-line lockup, unhyphenated `Distributed JBOD` |
| `djbod-lockup-stacked.svg` / `-onDark.svg` | square-ish contexts, splash, stickers |
| `djbod-mark.svg` | symbol on its own, ≥ 40 px |
| `djbod-mark-mono-dark.svg` / `-mono-white.svg` | one-colour print, embroidery, stamps |
| `djbod-icon-small.svg` | reduced 3-column mark, 24–64 px |
| `djbod-favicon.svg` | 2×2 reduction, ≤ 24 px |
| `djbod-appicon.svg` | dark rounded square, avatars / app tiles |
| `png/` | rasterised exports of all of the above |
| `proof.png`, `proof2.png`, `proof3.png` | contact sheets showing everything together |
| `proof-palette.png` | orange vs yellow parity, on both UI surfaces |

All type in the lockups is **converted to outlines**, so the SVGs render identically
without Segoe UI installed.

## Colour

| Role | Hex |
|---|---|
| Data slabs | `#2a78d6` (`#3987e5` on dark) — the UI's `--accent` |
| Parity slab | `#F97316` |
| Wordmark "JBOD" | `#0F172A` — `#F8FAFC` on dark |
| "DISTRIBUTED" | `#64748B` — `#94A3B8` on dark |
| App-icon background | `#0B1220` |

The blues are not chosen independently: they are the `--accent` values from
`crates/djbod-ui/ui.html`, light and dark, so the logo and the running app
are the same blue. If `--accent` ever changes, rebuild the kit with the new
value rather than letting the two drift.

### Yellow alternate

[`yellow/`](yellow/) is the whole kit with the parity slab in `#fab219` — the
UI's `--warning` — instead of orange. Same geometry, same blues. It is kept
for comparison; the orange set is the one in use.

## Usage rules

- **Which lockup:** the inline pair is the one to reach for in a wide space — a
  site header, a slide footer, a banner. The two-line `horizontal` lockup suits
  narrower spots where the full name on one line would have to be set small.
  Use the hyphenated inline version anywhere the repo name is meant (it matches
  `Distributed-JBOD`); the unhyphenated one reads better in prose settings.
- **Clear space:** one slab-height on every side (≈ 14 % of the mark's height).
- **Minimum sizes:** full mark 40 px · `icon-small` 24 px · `favicon` 16 px ·
  two-line horizontal lockup 120 px wide · inline lockup 200 px wide.
- Don't recolour the parity slab to match the others — that diagonal is the logo.
- Don't rebuild the wordmark in a different typeface; use the outlined SVGs.
- On photos or busy backgrounds use the mono or app-icon version.

## Rebuilding

`gen.awk` holds the geometry generator, `build.sh` drives Inkscape.

```bash
bash build.sh
```

Every colour is overridable, as is the output directory, so an alternate
palette can be rendered beside the primary kit:

```bash
OUT=yellow ORANGE='#fab219' bash build.sh
```

Set `INKSCAPE=/path/to/inkscape` if it isn't at the default Windows location.
Tweak the `stack(...)` arguments in `build.sh` to change column count, heights,
slab proportions or where the parity diagonal falls.

### Making a `.ico`

No ImageMagick on this machine, so the multi-resolution `favicon.ico` isn't built.
With ImageMagick installed:

```bash
magick png/djbod-favicon-16.png png/djbod-favicon-24.png png/djbod-favicon-32.png png/djbod-icon-48.png favicon.ico
```

## Alternative directions

Six earlier exploratory concepts are kept in [`concepts/`](concepts/README.md) for reference.
