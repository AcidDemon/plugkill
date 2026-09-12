#!/bin/sh
# Regenerates the SVG sources from the raster concept art in assets/GuardGoose.
# Run from assets/brand: ./vectorize.sh
#
# Needs ImageMagick, resvg and python3. Takes a couple of minutes.
#
# The concept art is NOT in the repo: it is several megabytes of raster to
# reproduce SVGs that are themselves committed, so it is kept outside. Drop
# 1.png and Icon_Firefox-Style.png into assets/GuardGoose to run this again.
# Day to day you do not need it: edit the SVGs directly and run ./build.sh,
# which only rasterizes.
#
# Why each step is shaped the way it is
# -------------------------------------
# The concept art is anti-aliased and gradient-shaded, so every boundary
# carries a band of in-between pixels. Those bands are the whole difficulty:
#
#   * A band can sit nearer the wrong palette entry than the right one. The
#     beak's lower edge passes through an olive that is closer to the ink than
#     to the orange, which is what turned the lower mandible dark.
#   * A band handed to the quantizer as its own colour becomes a real region,
#     which renders as grey speckle across the white plumage and as hairlines
#     along the lettering.
#
# So the ramp colours are named in the palette on purpose, and then folded into
# whichever neighbour they belong to at composition time. See trace.py for the
# layer-order and background-flood reasoning.
set -eu

SRC=../GuardGoose
T=./trace.py
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# --- icon: the G-form head, from Icon_Firefox-Style.png -----------------------
# The plate is not traced. Its anti-aliased rounded corners come out notched,
# and worse, the corner band snaps to white and becomes a loop enclosing the
# whole mark, which under fill-rule="evenodd" inverts the goose and hollows it
# out. So everything outside the plate is flooded with plate ink first, killing
# that boundary, and the plate is redrawn as a real rect with the art clipped.
magick "$SRC/Icon_Firefox-Style.png" -crop 620x620+315+45 +repage -resize 760x760 "$tmp/hero.png"
magick -size 760x760 xc:black -fill white -draw 'roundrectangle 6,6 753,753 168,168' "$tmp/pmask.png"
magick "$tmp/hero.png" \( -size 760x760 xc:'#162028' \) \( "$tmp/pmask.png" -negate \) \
  -compose over -composite "$tmp/icon-src.png"
python3 "$T" "$tmp/icon-src.png" "$tmp/icon-raw.svg" \
  'INK:162028,WHITE:f2f3f5,TEAL:01dfe0,ORANGE:fc990b' \
  --base INK --median 3 --eps 1.0 --minarea 18 --smoothmask 2

# --- mascot: the standing goose, from 1.png ----------------------------------
# The tagline text and the old cable are flooded out. The breast badge needs
# two passes: trace once to locate it, erase it, trace again. See deshield.py
# for why nothing cheaper works.
#
# Body white and page white are the same value here, separable only by
# connectivity, which is why --bgfill exists.
#
# The flood runs after the resize so every rectangle below is in the same
# 900-wide space as the beak tip at (696,149), which is what assemble.py
# anchors the drawn cable on.
#
# The cable cannot go by colour: it leaves the beak as one continuous dark
# shape with the beak's own outline, so a fill from inside it drains into
# every outline on the bird. It cannot go as one rectangle either, because
# below the plug the breast outline swings out into the same column the cable
# came down. So it goes as four steps, each one starting to the right of
# wherever the bird's own outline has reached by that row:
#
#   row 156   cable 660-681   neck   ...-  0    step 1 from 652
#   row 300   plug  702-738   neck   630-645
#   row 350   plug  711-807   breast 654-671   step 3 from 712
#   row 400   plug  761-826   breast 683-700   step 4 from 730
#
# The cut at row 156 leaves a short nub under the beak, which is the cable the
# goose is still holding; the banner's drawn stub starts there.
# No mid-grey entry. The art looks like it has one, but the goose's own
# shading is far lighter than a mid grey and snaps to the page anyway; what a
# mid-grey entry actually collects is the ink-to-page ramp. Those pixels are
# not background, so the silhouette swallowed them, and along the sole, where
# the ground shadow widens the ramp, they came out as a row of warts on a foot
# that is one smooth curve in the source. Without the entry they fall to the
# page and the foot traces clean.
# ORANGEEDGE is the light half of the ink-to-orange ramp and folds back into
# the orange. Without it that ramp has no entry of its own, and DARKEDGE, which
# exists for the ink-to-page ramp, is nearer to it than either the orange or
# the ink is, so the whole transition band went to the ink and the dark outline
# ate its way into the beak. What was left of the orange stopped above the
# mouth line, and the lower mandible came out as a fat white slit over a dark
# jaw instead of the orange it is in the art.
MPAL='BG:f7f8f9,INK:16212a,TEAL:03acb0,ORANGE:fc990b,DARKEDGE:42555c,ORANGEEDGE:c27b13'
magick "$SRC/1.png" -crop 760x720+55+85 +repage -resize 900x \
  -fill '#f7f8f9' \
  -draw 'rectangle 0,0 415,106' \
  -draw 'rectangle 652,156 900,305' \
  -draw 'rectangle 690,300 900,360' \
  -draw 'rectangle 712,355 900,400' \
  -draw 'rectangle 730,395 900,470' \
  "$tmp/mascot-badged.png"
python3 "$T" "$tmp/mascot-badged.png" "$tmp/mascot-locate.svg" "$MPAL" \
  --base INK --bgfill fdfdfd --median 3 --eps 1.1 --minarea 26
python3 deshield.py "$tmp/mascot-locate.svg" "$tmp/mascot-badged.png" "$tmp/mascot-src.png"
python3 "$T" "$tmp/mascot-src.png" "$tmp/mascot-raw.svg" "$MPAL" \
  --base INK --bgfill fdfdfd --median 3 --eps 1.1 --minarea 26 --smoothmask 2

# --- wordmark: "plugkill", from 1.png ----------------------------------------
# No base layer: a counter in the p or the g is page-coloured but enclosed, so
# the border flood cannot reach it and a silhouette would fill the hole solid.
# The top rows are flooded to clear the mascot's ground shadow, which otherwise
# traces as a smear above the letters.
magick "$SRC/1.png" -crop 690x175+70+793 +repage \
  -fill '#fcfdfd' -draw 'rectangle 0,0 690,9' -resize 1860x "$tmp/word-src.png"
python3 "$T" "$tmp/word-src.png" "$tmp/word-raw.svg" \
  'BG:fcfdfd,INK:19242b,TEAL:04a6ac,DARKEDGE:747b84,LIGHTEDGE:a4b6ba' \
  --base none --median 3 --eps 1.2 --minarea 40 --corner 34 --smoothmask 3

python3 assemble.py "$tmp"
printf 'regenerated: icon.svg mascot.svg wordmark.svg banner-light.svg banner-dark.svg glyph-mono.svg\n'
