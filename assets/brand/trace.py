#!/usr/bin/env python3
"""Trace flat-colour raster art into a layered SVG.

    trace.py SRC OUT PALETTE [--base NAME] [--scale S] [--eps E] [--minarea A]
             [--median N] [--bgfill HEX] [--corner DEG] [--smoothmask R]

PALETTE is a comma-separated list of NAME:hex entries in paint order, bottom
first. The entry named BG is the page behind the art: it is flood-filled from
the border and never emitted.

Why it works this way. The source art is anti-aliased, so every boundary
carries a band of in-between pixels. Feed those to a nearest-neighbour
quantizer and they become their own regions, which show up as grey speckle on
white and as fringes around letterforms. Worse, a transition colour can sit
nearer to the wrong palette entry than the right one: on the goose art the
orange beak's lower edge passes through an olive that is closer to the ink than
to the orange, which turned the lower mandible dark.

So the flattening happens before tracing, in ImageMagick: a median filter
collapses each transition band into whichever side dominates, then an exact
`-remap` with no dithering snaps what is left. By the time this script reads the
image every pixel is already one of the palette colours.

The other trap is layer order. Emitting a light silhouette first and painting
darker shapes over it leaves any seam showing as a light halo. Paint the ink
layer first instead, so a seam reads as outline, which is what the art wants
anyway.
"""

import math
import subprocess
import sys
from collections import defaultdict, deque


def parse_args(argv):
    if len(argv) < 4:
        sys.exit(__doc__)
    opts = {'base': None, 'scale': 1.0, 'eps': 1.2, 'minarea': 24, 'median': 5,
            'bgfill': None, 'corner': 32.0, 'smoothmask': 0}
    pos, i = [], 1
    while i < len(argv):
        a = argv[i]
        if a.startswith('--'):
            key = a[2:]
            if key not in opts:
                sys.exit(f'unknown option: {a}')
            opts[key] = argv[i + 1]
            i += 2
        else:
            pos.append(a)
            i += 1
    for k in ('scale', 'eps', 'minarea', 'corner'):
        opts[k] = float(opts[k])
    opts['median'] = int(opts['median'])
    opts['smoothmask'] = int(opts['smoothmask'])
    src, out, palspec = pos[0], pos[1], pos[2]
    pal = []
    for ent in palspec.split(','):
        name, _, hx = ent.partition(':')
        hx = hx.lstrip('#')
        pal.append((name, (int(hx[0:2], 16), int(hx[2:4], 16), int(hx[4:6], 16))))
    return src, out, pal, opts


def quantize(src, pal, median):
    """Flatten SRC to exactly the palette colours and return it as raw RGB."""
    swatches = ' '.join(f"xc:'rgb({r},{g},{b})'" for _, (r, g, b) in pal)
    palfile = '/tmp/.trace-pal.png'
    subprocess.run(f'magick -size 1x1 {swatches} +append {palfile}',
                   shell=True, check=True)
    cmd = ['magick', src]
    if median > 1:
        cmd += ['-statistic', 'Median', f'{median}x{median}']
    cmd += ['-dither', 'None', '-remap', palfile, '-depth', '8', 'ppm:-']
    raw = subprocess.run(cmd, capture_output=True, check=True).stdout

    def tok(buf, i):
        while buf[i:i + 1].isspace():
            i += 1
        if buf[i:i + 1] == b'#':
            while buf[i:i + 1] != b'\n':
                i += 1
            return tok(buf, i)
        j = i
        while not buf[j:j + 1].isspace():
            j += 1
        return buf[i:j], j

    _, i = tok(raw, 0)
    w, i = tok(raw, i)
    h, i = tok(raw, i)
    _, i = tok(raw, i)
    return int(w), int(h), raw[i + 1:]


def label(px, W, H, pal):
    """Map every pixel to its palette index. Exact after the remap; nearest is
    only a fallback for the stray pixel a filter may leave off-palette."""
    cols = [c for _, c in pal]
    exact = {c: i for i, c in enumerate(cols)}
    lab = bytearray(W * H)
    for k in range(W * H):
        rgb = (px[3 * k], px[3 * k + 1], px[3 * k + 2])
        idx = exact.get(rgb)
        if idx is None:
            best, idx = 1 << 30, 0
            for ci, (cr, cg, cb) in enumerate(cols):
                d = (rgb[0] - cr) ** 2 + (rgb[1] - cg) ** 2 + (rgb[2] - cb) ** 2
                if d < best:
                    best, idx = d, ci
        lab[k] = idx
    return lab


def background(lab, W, H, bgidx):
    """BG reachable from the border. Enclosed BG-coloured pockets stay art."""
    bg = bytearray(W * H)
    dq = deque()
    def seed(x, y):
        k = y * W + x
        if lab[k] == bgidx and not bg[k]:
            bg[k] = 1
            dq.append((x, y))
    for x in range(W):
        seed(x, 0); seed(x, H - 1)
    for y in range(H):
        seed(0, y); seed(W - 1, y)
    while dq:
        x, y = dq.popleft()
        for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1)):
            nx, ny = x + dx, y + dy
            if 0 <= nx < W and 0 <= ny < H:
                k = ny * W + nx
                if not bg[k] and lab[k] == bgidx:
                    bg[k] = 1
                    dq.append((nx, ny))
    return bg


def smooth_mask(mask, W, H, r):
    """Majority-filter a binary mask, which straightens a quantized edge.

    Hard-thresholding anti-aliased art puts the boundary wherever the ramp
    crosses 50 percent, and that crossing jitters by a pixel wherever the true
    edge runs nearly tangent to the pixel grid. Curve fitting cannot undo it:
    the wobble is in the mask, so it has to come out of the mask. Setting each
    pixel to the majority of its neighbourhood is the same as blurring and
    re-thresholding, and it costs one integral image rather than a pass per
    kernel cell.
    """
    if r < 1:
        return mask
    # integral image, (W+1) by (H+1) so every window is four lookups
    sat = [0] * ((W + 1) * (H + 1))
    for y in range(H):
        row, above, cur = y * W, y * (W + 1), (y + 1) * (W + 1)
        acc = 0
        for x in range(W):
            acc += mask[row + x]
            sat[cur + x + 1] = sat[above + x + 1] + acc
    out = bytearray(W * H)
    for y in range(H):
        y0, y1 = max(0, y - r), min(H - 1, y + r)
        top, bot = y0 * (W + 1), (y1 + 1) * (W + 1)
        row = y * W
        for x in range(W):
            x0, x1 = max(0, x - r), min(W - 1, x + r)
            total = (sat[bot + x1 + 1] - sat[bot + x0]
                     - sat[top + x1 + 1] + sat[top + x0])
            if total * 2 > (y1 - y0 + 1) * (x1 - x0 + 1):
                out[row + x] = 1
    return out


def contours(mask, W, H):
    """Boundary loops between set and unset pixels, art kept on the left."""
    edges = defaultdict(list)
    for y in range(H):
        row = y * W
        for x in range(W):
            if not mask[row + x]:
                continue
            if y == 0 or not mask[row - W + x]:
                edges[(x, y)].append((x + 1, y))
            if y == H - 1 or not mask[row + W + x]:
                edges[(x + 1, y + 1)].append((x, y + 1))
            if x == 0 or not mask[row + x - 1]:
                edges[(x, y + 1)].append((x, y))
            if x == W - 1 or not mask[row + x + 1]:
                edges[(x + 1, y)].append((x + 1, y + 1))
    loops = []
    while edges:
        start = next(iter(edges))
        loop, cur = [start], start
        while True:
            nxts = edges.get(cur)
            if not nxts:
                break
            nxt = nxts.pop()
            if not nxts:
                del edges[cur]
            loop.append(nxt)
            cur = nxt
            if cur == start:
                break
        if len(loop) > 8:
            loops.append(loop)
    return loops


def rdp_closed(pts, eps):
    """Douglas-Peucker on a closed ring, anchored at its extreme point."""
    if len(pts) < 5:
        return list(pts)
    a = min(range(len(pts)), key=lambda i: (pts[i][1], pts[i][0]))
    rot = pts[a:] + pts[:a]
    keep = rdp(rot + [rot[0]], eps)
    return keep[:-1] if keep[0] == keep[-1] else keep


def rdp(pts, eps):
    n = len(pts)
    if n < 3:
        return list(pts)
    keep = [False] * n
    keep[0] = keep[n - 1] = True
    stack = [(0, n - 1)]
    while stack:
        a, b = stack.pop()
        if b <= a + 1:
            continue
        x1, y1 = pts[a]
        x2, y2 = pts[b]
        dx, dy = x2 - x1, y2 - y1
        ln = math.hypot(dx, dy)
        dmax, idx = -1.0, -1
        for i in range(a + 1, b):
            x0, y0 = pts[i]
            d = (math.hypot(x0 - x1, y0 - y1) if ln < 1e-9
                 else abs(dy * x0 - dx * y0 + x2 * y1 - y2 * x1) / ln)
            if d > dmax:
                dmax, idx = d, i
        if dmax > eps and idx > 0:
            keep[idx] = True
            stack.append((a, idx))
            stack.append((idx, b))
    return [pts[i] for i in range(n) if keep[i]]


def _dist(a, b):
    return math.hypot(b[0] - a[0], b[1] - a[1])


def smooth_path(pts, s, corner_deg):
    """Emit the ring, smoothing only where the outline is genuinely curved.

    Catmull-Rom through every point rounds off the things that should stay
    crisp: a letter's straight stem bulges, a beak tip and a feather point go
    soft, and the result reads as wobble rather than as a cut edge. So each
    vertex whose turn exceeds corner_deg is marked a corner, and any segment
    touching one is emitted as a straight line. Curved runs still get the
    spline.
    """
    n = len(pts)
    if n < 3:
        return ''
    thresh = math.cos(math.radians(180.0 - corner_deg))

    corner = [False] * n
    for i in range(n):
        ax, ay = pts[i][0] - pts[(i - 1) % n][0], pts[i][1] - pts[(i - 1) % n][1]
        bx, by = pts[(i + 1) % n][0] - pts[i][0], pts[(i + 1) % n][1] - pts[i][1]
        la, lb = math.hypot(ax, ay), math.hypot(bx, by)
        if la < 1e-9 or lb < 1e-9:
            continue
        # cos of the angle between the incoming and outgoing directions;
        # straight ahead is 1, a hard turn tends to -1
        corner[i] = ((ax * bx + ay * by) / (la * lb)) < thresh

    d = [f'M{pts[0][0] * s:.2f} {pts[0][1] * s:.2f}']
    for i in range(n):
        p0, p1 = pts[(i - 1) % n], pts[i]
        p2, p3 = pts[(i + 1) % n], pts[(i + 2) % n]
        if corner[i] or corner[(i + 1) % n]:
            d.append(f'L{p2[0] * s:.2f} {p2[1] * s:.2f}')
            continue
        # Chordal Catmull-Rom: each tangent is scaled by how long its own
        # segment is, not by a fixed sixth of the span across it.
        # Simplification leaves wildly uneven segments, a four pixel corner
        # step next to a four hundred pixel straight run, and with a fixed
        # sixth the long side dominates the tangent and throws the control
        # point clear of the shape. That is where the hairline whiskers off the
        # letter corners came from. On evenly spaced points this reduces to the
        # same sixth, so nothing else moves.
        d01, d12 = _dist(p0, p1), _dist(p1, p2)
        d23 = _dist(p2, p3)
        t1 = d12 / (3.0 * (d01 + d12)) if d01 + d12 > 1e-9 else 0.0
        t2 = d12 / (3.0 * (d12 + d23)) if d12 + d23 > 1e-9 else 0.0
        c1 = (p1[0] + (p2[0] - p0[0]) * t1, p1[1] + (p2[1] - p0[1]) * t1)
        c2 = (p2[0] - (p3[0] - p1[0]) * t2, p2[1] - (p3[1] - p1[1]) * t2)
        d.append(f'C{c1[0] * s:.2f} {c1[1] * s:.2f} '
                 f'{c2[0] * s:.2f} {c2[1] * s:.2f} '
                 f'{p2[0] * s:.2f} {p2[1] * s:.2f}')
    d.append('Z')
    return ''.join(d)


def main():
    src, out, pal, opts = parse_args(sys.argv)
    names = [n for n, _ in pal]
    W, H, px = quantize(src, pal, opts['median'])
    lab = label(px, W, H, pal)
    bgidx = names.index('BG') if 'BG' in names else None
    bg = background(lab, W, H, bgidx) if bgidx is not None else bytearray(W * H)

    def region(idx):
        return [1 if (lab[k] == idx and not bg[k]) else 0 for k in range(W * H)]

    def paths(mask):
        mask = smooth_mask(mask, W, H, opts['smoothmask'])
        ps = []
        for loop in contours(mask, W, H):
            pts = loop[:-1] if loop[0] == loop[-1] else loop
            simp = rdp_closed(pts, opts['eps'])
            if len(simp) < 4:
                continue
            area = 0.0
            for i in range(len(simp)):
                x1, y1 = simp[i]
                x2, y2 = simp[(i + 1) % len(simp)]
                area += x1 * y2 - x2 * y1
            if abs(area) / 2 < opts['minarea']:
                continue
            ps.append(smooth_path(simp, opts['scale'], opts['corner']))
        return ''.join(ps)

    # Everything that is not background, painted in the base colour first, so a
    # seam between two layers shows as outline rather than as a light halo.
    #
    # `--base none` skips that layer and emits each colour region on its own.
    # Flat lettering needs this: a letter's counter is page-coloured but
    # enclosed, so the border flood cannot reach it, and a silhouette would
    # fill the hole in the p and the g solid.
    base = opts['base'] or (names[1] if len(names) > 1 else names[0])
    body = []
    if base != 'none':
        basecol = dict(pal)[base]
        silhouette = paths([0 if bg[k] else 1 for k in range(W * H)])
        body.append(f'    <path fill="#{basecol[0]:02x}{basecol[1]:02x}'
                    f'{basecol[2]:02x}" d="{silhouette}"/>')
    # In art where the subject's fill matches the page, the two are separable
    # only by connectivity: the page reaches the border, the subject's interior
    # does not. --bgfill paints those enclosed pockets, which is how the goose
    # gets a white body when its white is the same value as the paper.
    if opts['bgfill'] and bgidx is not None:
        d = paths(region(bgidx))
        if d:
            body.append(f'    <path fill="#{opts["bgfill"].lstrip("#")}" d="{d}"/>')

    for name, (r, g, b) in pal:
        if name == 'BG' or name == base:
            continue
        d = paths(region(names.index(name)))
        if d:
            body.append(f'    <path fill="#{r:02x}{g:02x}{b:02x}" d="{d}"/>')

    vw, vh = W * opts['scale'], H * opts['scale']
    svg = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {vw:.0f} {vh:.0f}" '
           f'width="{vw:.0f}" height="{vh:.0f}" role="img" aria-label="plugkill">\n'
           f'  <g fill-rule="evenodd">\n' + '\n'.join(body) + '\n  </g>\n</svg>\n')
    with open(out, 'w') as f:
        f.write(svg)
    print(f'{W}x{H} -> {out}  {len(svg) // 1024} KB  layers={len(body)}')


if __name__ == '__main__':
    main()
