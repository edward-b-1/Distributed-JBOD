# Palette exploration

Working comparisons from choosing the logo's colours. These are not assets —
the finished kits are in the parent directory. They are kept because the
comparisons record *why* each colour was picked, which is harder to recover
later than the files themselves.

The SVGs are the sources; each PNG beside one is its render. The SVGs in
`yellow-candidates/` reference the small PNGs by relative path, so keep the
files together.

## blue-candidates/

`blue-and-accent-candidates.png` — the mark on the UI's own surfaces
(`#f5f5f3` light, `#121211` dark), four columns:

1. the original logo blue `#0EA5E9` + orange `#F97316`
2. the UI accent `#2a78d6` + the same orange — what shipped
3. the UI accent + `--serious` `#ec835a` — rejected, too washed out to carry
   the parity diagonal
4. the UI accent + `--warning` `#fab219` — became `../yellow/`

## yellow-candidates/

`five-yellows.png` — `#fab219`, `#FFC107`, `#FFC61A`, `#FFD21A`, `#FFDD2E` on
both surfaces. `#FFDD2E` is brightest but starts losing contrast against the
light surface, so `#FFD21A` won.

`flat-vs-glow-vs-gradient.png` — three ways to make `#FFD21A` look lit: flat,
a soft outer glow, and a "lit dome" gradient. The gradient was too subtle to
justify its complexity; the glow shipped as `../status-light/`.

`glow-at-small-sizes.png` and `glow-icon-*.png` — the glow at 64 / 48 / 32 /
16 px, checking it degrades gracefully. Below roughly 32 px it stops reading
as a glow and simply warms the colour.

`grouped-parity.awk` — a throwaway variant of `stack()` that emitted the
parity slabs in their own group so a filter could be tried on them. The idea
graduated into the optional `lit` parameter in `../gen.awk`; this is the
prototype it came from.

`glow-icon-source.svg` — the reduced 3-column mark used for the size test.
