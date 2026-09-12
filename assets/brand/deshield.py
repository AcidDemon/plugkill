#!/usr/bin/env python3
"""Erase the breast badge from the mascot source before the real trace.

    deshield.py RAW_SVG SRC_PNG OUT_PNG

Called twice-over by vectorize.sh: the mascot is traced once to find the badge,
this erases it from the raster, and the result is traced again for real.

Why it takes two passes. The badge is not separable by colour, because its dark
field is the same ink as the body outline, and it is not separable by rectangle,
because the body outline passes within a few pixels of its right edge. Every
cheaper attempt failed a specific way worth recording: deleting its subpaths in
vector leaves the ink base showing through as a dark badge silhouette; covering
that with a stroked copy of its own outline leaves a ghost keyline; a raster
flood-fill of the dark field leaks through the anti-aliased boundary and eats
the goose's own outlines.

What works is a badge-shaped mask. The first trace already found the badge's
outer ring, so that path, grown by a stroke to swallow the keyline around it,
is exactly the region to repaint and nothing else. Painting it in the page
colour makes the breast continuous, and since body white and page white are the
same value here, the second trace sees uninterrupted plumage with no seam.
"""

import pathlib
import re
import subprocess
import sys

# The badge sits on the breast. These bounds only have to be loose enough to
# contain it and tight enough to exclude the teal wing flashes and the feet.
BREAST = (480, 330, 760, 620)


def subpaths(d):
    return ['M' + s for s in d.split('M') if s.strip()]


def bbox(p):
    n = [float(x) for x in re.findall(r'-?\d+\.?\d*', p)]
    xs, ys = n[0::2], n[1::2]
    return min(xs), min(ys), max(xs), max(ys)


def main():
    raw, src, out = (pathlib.Path(a) for a in sys.argv[1:4])
    layers = dict(re.findall(r'<path fill="(#[0-9a-f]{6})" d="([^"]*)"/>',
                             raw.read_text()))
    m = re.search(r'viewBox="0 0 (\d+) (\d+)"', raw.read_text())
    if not m:
        raise SystemExit(f'no viewBox in {raw}')
    W, H = int(m.group(1)), int(m.group(2))

    x0b, y0b, x1b, y1b = BREAST
    ring, area = None, 0.0
    for p in subpaths(layers.get('#03acb0', '')):
        x0, y0, x1, y1 = bbox(p)
        if x0b < x0 and x1 < x1b and y0b < y0 and y1 < y1b:
            a = (x1 - x0) * (y1 - y0)
            if a > area:
                ring, area = p, a
    if ring is None:
        raise SystemExit('badge ring not found on the breast; check BREAST bounds')

    tmp = out.parent
    mask_svg, mask_png, patch = tmp / '_mask.svg', tmp / '_mask.png', tmp / '_patch.png'
    mask_svg.write_text(
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" '
        f'width="{W}" height="{H}">'
        f'<rect width="{W}" height="{H}" fill="black"/>'
        f'<path fill="white" stroke="white" stroke-width="26" '
        f'stroke-linejoin="round" d="{ring}"/></svg>')
    subprocess.run(['resvg', '-w', str(W), str(mask_svg), str(mask_png)], check=True)
    # mask as alpha on a flat page-coloured layer, then composite. Doing this
    # explicitly rather than via a three-image composite, whose mask semantics
    # silently repainted the whole breast when this was first written.
    subprocess.run(['magick', '(', '-size', f'{W}x{H}', 'xc:#f7f8f9', ')',
                    '(', str(mask_png), '-colorspace', 'gray', ')',
                    '-alpha', 'off', '-compose', 'CopyOpacity', '-composite',
                    str(patch)], check=True)
    subprocess.run(['magick', str(src), str(patch), '-compose', 'over',
                    '-composite', str(out)], check=True)
    x0, y0, x1, y1 = bbox(ring)
    print(f'badge erased: ring ({x0:.0f},{y0:.0f})-({x1:.0f},{y1:.0f})')


if __name__ == '__main__':
    main()
