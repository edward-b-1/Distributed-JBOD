#!/usr/bin/env bash
# Regenerates every Distributed JBOD logo asset from scratch.
#   usage: bash build.sh
#
# The palette mirrors the UI's CSS variables in crates/djbod-ui/ui.html:
# BLUE/BLUE_D are --accent light/dark, so the logo and the app agree.
# Override any of them, and OUT, to render the kit in another palette:
#   OUT=yellow ORANGE='#fab219' bash build.sh     # --warning instead of orange
set -e
IK="${INKSCAPE:-/c/Program Files/Inkscape/bin/inkscape.exe}"
BLUE="${BLUE:-#2a78d6}";   ORANGE="${ORANGE:-#F97316}"
BLUE_D="${BLUE_D:-#3987e5}"
INK="#0F172A"; MUTED="#64748B"; INK_D="#F8FAFC"; MUTED_D="#94A3B8"; BG_D="#0B1220"

# Everything is written under OUT so an alternate palette can live beside the
# primary kit; gen.awk and the proof sheets stay in the kit root.
OUT="${OUT:-.}"
GEN="$PWD/gen.awk"
mkdir -p "$OUT/png"
if [ "$OUT" != "." ]; then cp proof.svg proof2.svg proof3.svg "$OUT/"; fi
cd "$OUT"

# --- the mark: 4 columns of pill-shaped slabs, heights 4/6/3/5 (mixed capacity),
#     with a parity slab descending diagonally across them (levels 3/2/1/0).
mark () { # $1=out $2=blue $3=orange
  awk -f "$GEN" -e "BEGIN{
    printf \"<svg xmlns=\\\"http://www.w3.org/2000/svg\\\" width=\\\"256\\\" height=\\\"256\\\" viewBox=\\\"0 0 256 256\\\">\n  <title>Distributed JBOD</title>\n\";
    printf \"%s\", stack(4,44,23,10,8,\"4 6 3 5\",\"3 2 1 0\",\"$2\",\"$3\",256);
    printf \"</svg>\n\";
  }" > "$1"
}
mark djbod-mark.svg            "$BLUE"    "$ORANGE"
mark djbod-mark-mono-dark.svg  "$INK"     "$INK"
mark djbod-mark-mono-white.svg "#FFFFFF"  "#FFFFFF"

# --- reduced marks for small sizes
awk -f "$GEN" -e "BEGIN{
  printf \"<svg xmlns=\\\"http://www.w3.org/2000/svg\\\" width=\\\"256\\\" height=\\\"256\\\" viewBox=\\\"0 0 256 256\\\">\n  <title>Distributed JBOD</title>\n\";
  printf \"%s\", stack(3,56,40,18,14,\"3 2 4\",\"2 1 0\",\"$BLUE\",\"$ORANGE\",256);
  printf \"</svg>\n\";
}" > djbod-icon-small.svg
awk -f "$GEN" -e "BEGIN{
  printf \"<svg xmlns=\\\"http://www.w3.org/2000/svg\\\" width=\\\"256\\\" height=\\\"256\\\" viewBox=\\\"0 0 256 256\\\">\n  <title>Distributed JBOD</title>\n\";
  printf \"%s\", stack(2,96,66,24,24,\"2 2\",\"1 0\",\"$BLUE\",\"$ORANGE\",256);
  printf \"</svg>\n\";
}" > djbod-favicon.svg
awk -f "$GEN" -e "BEGIN{
  printf \"<svg xmlns=\\\"http://www.w3.org/2000/svg\\\" width=\\\"512\\\" height=\\\"512\\\" viewBox=\\\"0 0 512 512\\\">\n  <title>Distributed JBOD</title>\n\";
  printf \"  <rect width=\\\"512\\\" height=\\\"512\\\" rx=\\\"112\\\" fill=\\\"$BG_D\\\"/>\n\";
  printf \"  <g transform=\\\"translate(256,256) scale(1.42) translate(-128,-128)\\\">\n\";
  printf \"%s\", stack(3,56,40,18,14,\"3 2 4\",\"2 1 0\",\"$BLUE_D\",\"$ORANGE\",256);
  printf \"  </g>\n</svg>\n\";
}" > djbod-appicon.svg

# --- lockups (type set live, then outlined by Inkscape so the files need no fonts)
lockup_h () { awk -f "$GEN" -e "BEGIN{
  s=96/178;
  printf \"<svg xmlns=\\\"http://www.w3.org/2000/svg\\\" width=\\\"338\\\" height=\\\"144\\\" viewBox=\\\"0 8 338 144\\\">\n  <title>Distributed JBOD</title>\n\";
  printf \"  <g transform=\\\"translate(24,32) scale(%g) translate(-25,-39)\\\">\n\", s;
  printf \"%s\", stack(4,44,23,10,8,\"4 6 3 5\",\"3 2 1 0\",\"$BLUE\",\"$ORANGE\",256);
  printf \"  </g>\n  <g transform=\\\"translate(24,32)\\\">\n\";
  printf \"    <text x=\\\"137.3\\\" y=\\\"26\\\" font-family=\\\"Segoe UI Semibold\\\" font-size=\\\"19\\\" letter-spacing=\\\"3.66\\\" fill=\\\"$2\\\">DISTRIBUTED</text>\n\";
  printf \"    <text x=\\\"138\\\" y=\\\"84\\\" font-family=\\\"Segoe UI\\\" font-weight=\\\"700\\\" font-size=\\\"60\\\" letter-spacing=\\\"-0.5\\\" fill=\\\"$3\\\">JBOD</text>\n\";
  printf \"  </g>\n</svg>\n\";
}" > "$1"; }

lockup_v () { awk -f "$GEN" -e "BEGIN{
  s=120/178; mw=206*s; x0=(198-mw)/2;
  printf \"<svg xmlns=\\\"http://www.w3.org/2000/svg\\\" width=\\\"198\\\" height=\\\"270\\\" viewBox=\\\"0 0 198 270\\\">\n  <title>Distributed JBOD</title>\n\";
  printf \"  <g transform=\\\"translate(%g,24) scale(%g) translate(-25,-39)\\\">\n\", x0, s;
  printf \"%s\", stack(4,44,23,10,8,\"4 6 3 5\",\"3 2 1 0\",\"$BLUE\",\"$ORANGE\",256);
  printf \"  </g>\n\";
  printf \"  <text x=\\\"99\\\" y=\\\"196\\\" text-anchor=\\\"middle\\\" font-family=\\\"Segoe UI Semibold\\\" font-size=\\\"19\\\" letter-spacing=\\\"3.66\\\" fill=\\\"$2\\\">DISTRIBUTED</text>\n\";
  printf \"  <text x=\\\"99\\\" y=\\\"246\\\" text-anchor=\\\"middle\\\" font-family=\\\"Segoe UI\\\" font-weight=\\\"700\\\" font-size=\\\"60\\\" letter-spacing=\\\"-0.5\\\" fill=\\\"$3\\\">JBOD</text>\n\";
  printf \"</svg>\n\";
}" > "$1"; }

lockup_h _h-light.svg "$MUTED"   "$INK"
lockup_h _h-dark.svg  "$MUTED_D" "$INK_D"
lockup_v _v-light.svg "$MUTED"   "$INK"
lockup_v _v-dark.svg  "$MUTED_D" "$INK_D"

outline () { "$IK" --actions="select-all:all;object-to-path;export-filename:$2;export-plain-svg;export-overwrite;export-do" "$1" >/dev/null 2>&1; }
outline _h-light.svg djbod-lockup-horizontal.svg
outline _h-dark.svg  djbod-lockup-horizontal-onDark.svg
outline _v-light.svg djbod-lockup-stacked.svg
outline _v-dark.svg  djbod-lockup-stacked-onDark.svg
rm -f _h-light.svg _h-dark.svg _v-light.svg _v-dark.svg

# --- inline lockups: mark left, the name on one line, hyphenated or not
inline_src () { # $1=out  $2=joiner entity  $3=muted colour  $4=ink colour  $5=title
awk -f "$GEN" -e "BEGIN{
  mh=76; s=mh/178; mw=206*s; fs=48; cap=fs*0.7;
  yb = 24 + mh/2 + cap/2; tx = 24 + mw + 26; h = mh + 48;
  printf \"<svg xmlns=\\\"http://www.w3.org/2000/svg\\\" width=\\\"900\\\" height=\\\"%g\\\" viewBox=\\\"0 0 900 %g\\\">\n  <title>$5</title>\n\", h, h;
  printf \"  <g transform=\\\"translate(24,24) scale(%g) translate(-25,-39)\\\">\n\", s;
  printf \"%s\", stack(4,44,23,10,8,\"4 6 3 5\",\"3 2 1 0\",\"$BLUE\",\"$ORANGE\",256);
  printf \"  </g>\n\";
  printf \"  <text x=\\\"%g\\\" y=\\\"%g\\\" font-family=\\\"Segoe UI\\\" font-size=\\\"%g\\\" letter-spacing=\\\"-0.5\\\">\", tx, yb, fs;
  printf \"<tspan font-weight=\\\"400\\\" fill=\\\"$3\\\">Distributed$2</tspan>\";
  printf \"<tspan font-weight=\\\"700\\\" fill=\\\"$4\\\">JBOD</tspan></text>\n</svg>\n\";
}" > "$1"; }

# outline the type, then tighten the 900-wide working canvas to the drawing + 24px
finish () { # $1=src $2=out
  outline "$1" "$2"
  W=$("$IK" --query-all "$2" 2>/dev/null | head -1 | awk -F, '{printf "%.0f", $2+$4+24}')
  sed -i -E "s/width=\"900(\.[0-9]+)?\"/width=\"$W\"/; s/viewBox=\"0 0 900(\.[0-9]+)? /viewBox=\"0 0 $W /" "$2"
  rm -f "$1"
}
inline_src _i1.svg "-"      "$MUTED"   "$INK"   "Distributed-JBOD"; finish _i1.svg djbod-lockup-inline.svg
inline_src _i2.svg "-"      "$MUTED_D" "$INK_D" "Distributed-JBOD"; finish _i2.svg djbod-lockup-inline-onDark.svg
inline_src _i3.svg "&#160;" "$MUTED"   "$INK"   "Distributed JBOD"; finish _i3.svg djbod-lockup-inline-nohyphen.svg
inline_src _i4.svg "&#160;" "$MUTED_D" "$INK_D" "Distributed JBOD"; finish _i4.svg djbod-lockup-inline-nohyphen-onDark.svg

# --- raster exports
png () { "$IK" --export-type=png --export-filename="$2" -w "$3" "$1" >/dev/null 2>&1; }
for s in 512 256 128 64;  do png djbod-mark.svg      "png/djbod-mark-$s.png"      $s; done
for s in 64 48 32 16;     do png djbod-icon-small.svg "png/djbod-icon-$s.png"     $s; done
for s in 32 24 16;        do png djbod-favicon.svg   "png/djbod-favicon-$s.png"   $s; done
for s in 512 256 128 96;  do png djbod-appicon.svg   "png/djbod-appicon-$s.png"   $s; done
png djbod-mark-mono-dark.svg           png/djbod-mark-mono-dark-512.png   512
png djbod-mark-mono-white.svg          png/djbod-mark-mono-white-512.png  512
png djbod-lockup-horizontal.svg        png/djbod-lockup-horizontal-1024.png 1024
png djbod-lockup-horizontal.svg        png/djbod-lockup-horizontal-512.png   512
png djbod-lockup-horizontal-onDark.svg png/djbod-lockup-horizontal-onDark-1024.png 1024
png djbod-lockup-stacked.svg           png/djbod-lockup-stacked-600.png        600
png djbod-lockup-stacked-onDark.svg    png/djbod-lockup-stacked-onDark-600.png 600
for f in djbod-lockup-inline djbod-lockup-inline-nohyphen; do
  png $f.svg              "png/$f-1024.png"        1024
  png $f.svg              "png/$f-512.png"          512
  png $f-onDark.svg       "png/$f-onDark-1024.png" 1024
done
png proof.svg  proof.png  1180
png proof2.svg proof2.png  700
png proof3.svg proof3.png  620
echo "built $(ls png | wc -l) PNGs and $(ls *.svg | grep -v proof | wc -l) SVGs"
