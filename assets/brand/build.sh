#!/bin/sh
# Builds the plugkill lockups from the mascot + wordmark sources.
# Run from assets/brand: ./build.sh
set -eu
inner() { sed -e '1d' -e '$d' "$1"; }

WATCH=$(inner mascot-watchdog.svg)
WORD_DUO_D=$(inner wordmark-duo.svg)
WORD_DUO_L=$(inner wordmark-duo.svg | sed 's/#f0f6fc/#1a2429/')

tag() { printf '<text x="0" y="0" font-family="JetBrains Mono, DejaVu Sans Mono, monospace" font-size="30" letter-spacing="3" fill="#109fa6">Every bus watched. One answer.</text>'; }

lockup() {
  cat <<SVG
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1260 490" width="1260" height="490" role="img" aria-label="plugkill">
  <g transform="translate(30,20) scale(0.63)">$WATCH</g>
  <g transform="translate(556.25,81.25) scale(0.85)">$1</g>
  <g transform="translate(541,378)">$(tag)</g>
</svg>
SVG
}

# Crops a generated SVG's viewBox to what is actually drawn, so the README
# logo has no dead band above or below it. Needs resvg + ImageMagick.
tighten() {
  f=$1; pad=${2:-6}
  vb=$(sed -n 's/.*viewBox="\([^"]*\)".*/\1/p' "$f" | head -1)
  vx=$(echo "$vb" | cut -d' ' -f1); vy=$(echo "$vb" | cut -d' ' -f2)
  vw=$(echo "$vb" | cut -d' ' -f3); vh=$(echo "$vb" | cut -d' ' -f4)
  png=$(mktemp /tmp/pk-tight-XXXXXX.png)
  resvg -w 2000 "$f" "$png" 2>/dev/null
  box=$(magick "$png" -format "%@" info:)
  rm -f "$png"
  bw=${box%%x*}; rest=${box#*x}; bh=${rest%%+*}; rest=${rest#*+}; bx=${rest%%+*}; by=${rest#*+}
  nvb=$(awk -v vx="$vx" -v vy="$vy" -v vw="$vw" -v bw="$bw" -v bh="$bh" -v bx="$bx" -v by="$by" -v p="$pad" \
    'BEGIN{s=vw/2000; printf "%.1f %.1f %.1f %.1f", vx+bx*s-p, vy+by*s-p, bw*s+2*p, bh*s+2*p}')
  w=$(echo "$nvb" | cut -d' ' -f3); h=$(echo "$nvb" | cut -d' ' -f4)
  sed -i "1s|viewBox=\"[^\"]*\" width=\"[^\"]*\" height=\"[^\"]*\"|viewBox=\"$nvb\" width=\"$w\" height=\"$h\"|" "$f"
}

lockup "$WORD_DUO_D" > logo-watchdog-dark.svg
lockup "$WORD_DUO_L" > logo-watchdog-light.svg
for f in logo-watchdog-dark.svg logo-watchdog-light.svg; do
  [ -f "$f" ] && tighten "$f"
done

{ head -1 wordmark.svg; inner wordmark.svg | sed 's/#f0f6fc/#14161b/'; echo '</svg>'; } > wordmark-light.svg
