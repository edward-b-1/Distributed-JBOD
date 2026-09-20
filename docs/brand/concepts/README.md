# Logo concepts — alternative directions

Six exploratory directions for the Distributed-JBOD mark, kept for reference.
The adopted logo lives one level up in [`docs/brand/`](../README.md); nothing here
is used by the app.

Every concept has the same brief: **drives of mixed size, spread over ordinary
machines, pooled into one volume, with Reed-Solomon parity keeping it whole.**
Each mark tries to say that without a caption.

| # | Name | Idea | Accent |
|---|---|---|---|
| 01 | Mosaic Pool | Blocks of unequal size packed into one square. Two accent blocks are the parity shards. | `#0e7c7b` |
| 02 | Lattice | Six nodes of different weight tied into a hexagonal mesh around a shared core. | `#3b4fd8` |
| 03 | Stripe D | The letter D cut into five stripes, like a file striped across drives; one stripe is parity. | `#d9662b` |
| 04 | Bunch of Disks | Four cylinders of unequal height hanging off one bus line. The literal "bunch of disks". | `#0e7c7b` |
| 05 | Block Hyphen | Type-led wordmark; the hyphen becomes a 2×2 block grid with one parity block. | `#d9662b` |
| 06 | Erasure D | A dot-matrix D from dots of mixed size that still reads with two dots missing. | `#b8322f` |

## Files

```
concepts/
├── svg/       standalone SVGs, one mark + one lockup per concept, light and -onDark
├── png/       rasterised exports of every SVG (marks at 128 / 256 / 512, lockups at 1024 wide)
└── canvas/    the editable presentation boards (see below)
```

`svg/concept-NN-<name>-mark.svg` is the symbol alone on a 160×160 viewBox with
transparent background. `-lockup.svg` puts it beside the wordmark. Concept 05 has
`-wordmark.svg` instead of a lockup because the type *is* the mark.

The PNGs in `png/` were exported with Inkscape 1.4 from the SVGs, for anywhere SVG
is not accepted. Mark PNGs keep the transparent background. The lockup PNGs were
rendered on a machine without Space Grotesk, so their type is the Segoe UI fallback;
re-export after installing the font for the intended face.

Shared system across all six:

| Role | Value |
|---|---|
| Ink | `#16181d` |
| Ground | `#f4f3ee` |
| Wordmark | Space Grotesk, "Distributed" at 500 weight, "JBOD" at 700 |
| Labels | IBM Plex Mono |

The lockup and wordmark SVGs use **live text**, not outlines, so they need Space
Grotesk installed (free from Google Fonts) to render as intended; they fall back to
Segoe UI otherwise. The marks themselves have no text and render anywhere.

## The canvas boards

`canvas/` holds the six presentation artboards (`*.dc.html`) and their layout
(`canvas.json`) from the Claude design canvas where these were drawn. Each board
shows the mark large, light and dark lockups, and the mark at 48 / 32 / 20 px, with
the accent colour as an adjustable tweak. They are plain HTML with inline SVG, so a
browser will open one directly, but the `support.js` reference and the `{{accent}}`
placeholders only resolve inside the canvas editor. Treat them as source, not as
deliverables.
