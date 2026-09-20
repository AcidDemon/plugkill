#!/bin/sh
# Rebuilds the plugkill PNG exports from the SVG sources in this directory.
# Run from assets/brand: ./build.sh
#
# Needs resvg (SVG rasterizer) and ImageMagick.
#
# The SVG sources are the masters. Everything under png/ is generated, so edit
# the SVGs and re-run rather than touching the PNGs.
#
# icon.svg        plated app icon, the G-form mark on its own dark plate
# glyph-mono.svg  single path, fill="currentColor", for the tray
# tray-icons.py   generates crates/plugkill-gui/icons, the symbolic tray icons
# mascot.svg      full-body goose, no cable (the banner draws the cable)
# wordmark.svg    "plugkill" lettering
# banner-*.svg    wide README lockups, mascot left and wordmark right
set -eu

out=png
mkdir -p "$out"

# App icon. 22 is in the list because that is what the GTK/ksni tray asks for.
for s in 16 22 24 32 48 64 128 256 512; do
  resvg -w "$s" icon.svg "$out/icon-$s.png"
done

# Coloured glyph PNGs, one per daemon state, for documentation and previews.
# The tray itself uses the symbolic icons from tray-icons.py.
state_colour() {
  case "$1" in
    armed)        printf '#2ecc40' ;;
    learning)     printf '#f5c211' ;;
    disarmed)     printf '#e04f5f' ;;
    disconnected) printf '#888888' ;;
  esac
}
for st in armed learning disarmed disconnected; do
  c=$(state_colour "$st")
  sed "s/currentColor/$c/" glyph-mono.svg > "$out/.glyph-$st.svg"
  for s in 22 24 32 48; do
    resvg -w "$s" "$out/.glyph-$st.svg" "$out/glyph-$st-$s.png"
  done
  rm -f "$out/.glyph-$st.svg"
done

# README banners. 1440 wide matches the SVG viewBox; 720 is the 2x-friendly
# half for a README that renders at roughly 720 CSS px.
for v in light dark; do
  resvg -w 1440 "banner-$v.svg" "$out/banner-$v.png"
  resvg -w 720 "banner-$v.svg" "$out/banner-$v@1x.png"
done

# Favicon, the sizes a browser actually picks from.
magick "$out/icon-16.png" "$out/icon-32.png" "$out/icon-48.png" "$out/favicon.ico"

printf 'wrote %s files to %s/\n' "$(ls -1 "$out" | wc -l | tr -d ' ')" "$out"
