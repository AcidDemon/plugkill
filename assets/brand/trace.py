import subprocess, sys, math
from collections import deque, defaultdict

SRC = sys.argv[1]; OUT = sys.argv[2]
SCALE = float(sys.argv[3]) if len(sys.argv) > 3 else 1.0
EPS = float(sys.argv[4]) if len(sys.argv) > 4 else 1.4
MINA = float(sys.argv[5]) if len(sys.argv) > 5 else 30

raw = subprocess.run(['magick', SRC, '-depth', '8', 'ppm:-'], capture_output=True).stdout
# parse P6
def tok(buf, i):
    while buf[i:i+1].isspace(): i += 1
    if buf[i:i+1] == b'#':
        while buf[i:i+1] != b'\n': i += 1
        return tok(buf, i)
    j = i
    while not buf[j:j+1].isspace(): j += 1
    return buf[i:j], j
p, i = tok(raw, 0); w, i = tok(raw, i); h, i = tok(raw, i); mx, i = tok(raw, i)
W, H = int(w), int(h); i += 1
px = raw[i:]

PAL = {
 'INK':   (0x1b,0x24,0x28),
 'WHITE': (0xfb,0xfb,0xfb),
 'GRAY':  (0xb6,0xc4,0xca),
 'TEAL':  (0x28,0xce,0xd3),
 'DEEP':  (0x1a,0xa0,0xa8),
}
names = list(PAL)
cols = [PAL[n] for n in names]

lab = bytearray(W*H)
for k in range(W*H):
    r, g, b = px[3*k], px[3*k+1], px[3*k+2]
    best, bi = 1<<30, 0
    for ci, (cr, cg, cb) in enumerate(cols):
        d = (r-cr)**2 + (g-cg)**2 + (b-cb)**2
        if d < best: best, bi = d, ci
    lab[k] = bi

IWHITE = names.index('WHITE')

# despeckle: 3x3 majority, two passes
for _ in range(2):
    nl = bytearray(lab)
    for y in range(1, H-1):
        for x in range(1, W-1):
            cnt = defaultdict(int)
            for dy in (-1,0,1):
                for dx in (-1,0,1):
                    cnt[lab[(y+dy)*W + x+dx]] += 1
            v, c = max(cnt.items(), key=lambda kv: kv[1])
            if c >= 6: nl[y*W+x] = v
    lab = nl

# background = WHITE connected to border
bg = bytearray(W*H)
dq = deque()
for x in range(W):
    for y in (0, H-1):
        if lab[y*W+x] == IWHITE and not bg[y*W+x]: bg[y*W+x] = 1; dq.append((x,y))
for y in range(H):
    for x in (0, W-1):
        if lab[y*W+x] == IWHITE and not bg[y*W+x]: bg[y*W+x] = 1; dq.append((x,y))
while dq:
    x, y = dq.popleft()
    for dx, dy in ((1,0),(-1,0),(0,1),(0,-1)):
        nx, ny = x+dx, y+dy
        if 0 <= nx < W and 0 <= ny < H and not bg[ny*W+nx] and lab[ny*W+nx] == IWHITE:
            bg[ny*W+nx] = 1; dq.append((nx,ny))

def mask_of(pred):
    return [1 if pred(lab[y*W+x], bg[y*W+x]) else 0 for y in range(H) for x in range(W)]

def contours(mask):
    """Trace boundary edges between filled/empty pixels; return closed loops of points."""
    edges = defaultdict(list)   # directed unit edges keeping filled area on the left
    def add(a, b): edges[a].append(b)
    for y in range(H):
        row = y*W
        for x in range(W):
            if not mask[row+x]: continue
            # up neighbour empty -> edge (x,y)->(x+1,y)
            if y == 0 or not mask[row-W+x]: add((x,y),(x+1,y))
            if y == H-1 or not mask[row+W+x]: add((x+1,y+1),(x,y+1))
            if x == 0 or not mask[row+x-1]: add((x,y+1),(x,y))
            if x == W-1 or not mask[row+x+1]: add((x+1,y),(x+1,y+1))
    loops = []
    while edges:
        start = next(iter(edges))
        loop = [start]; cur = start
        while True:
            nxts = edges.get(cur)
            if not nxts:
                break
            nxt = nxts.pop()
            if not nxts: del edges[cur]
            loop.append(nxt); cur = nxt
            if cur == start: break
        if len(loop) > 8: loops.append(loop)
    return loops

def rdp(pts, eps):
    """Iterative Douglas-Peucker on an open polyline."""
    n = len(pts)
    if n < 3: return list(pts)
    keep = [False]*n
    keep[0] = keep[n-1] = True
    stack = [(0, n-1)]
    while stack:
        a, b = stack.pop()
        if b <= a+1: continue
        x1, y1 = pts[a]; x2, y2 = pts[b]
        dx, dy = x2-x1, y2-y1
        ln = math.hypot(dx, dy)
        dmax, idx = -1.0, -1
        for i in range(a+1, b):
            x0, y0 = pts[i]
            if ln < 1e-9:
                d = math.hypot(x0-x1, y0-y1)
            else:
                d = abs(dy*x0 - dx*y0 + x2*y1 - y2*x1)/ln
            if d > dmax: dmax, idx = d, i
        if dmax > eps and idx > 0:
            keep[idx] = True
            stack.append((a, idx)); stack.append((idx, b))
    return [pts[i] for i in range(n) if keep[i]]

def rdp_closed(pts, eps):
    """Douglas-Peucker on a closed ring: split at two opposite points first."""
    n = len(pts)
    if n < 6: return list(pts)
    i1 = n//2
    a = rdp(pts[0:i1+1], eps)
    b = rdp(pts[i1:] + [pts[0]], eps)
    return a[:-1] + b[:-1]

def smooth_path(pts, s=1.0):
    """Closed Catmull-Rom -> cubic beziers."""
    n = len(pts)
    if n < 3: return ''
    d = [f'M{pts[0][0]*s:.1f} {pts[0][1]*s:.1f}']
    for i in range(n):
        p0 = pts[(i-1) % n]; p1 = pts[i]; p2 = pts[(i+1) % n]; p3 = pts[(i+2) % n]
        c1 = (p1[0] + (p2[0]-p0[0])/6.0, p1[1] + (p2[1]-p0[1])/6.0)
        c2 = (p2[0] - (p3[0]-p1[0])/6.0, p2[1] - (p3[1]-p1[1])/6.0)
        d.append(f'C{c1[0]*s:.1f} {c1[1]*s:.1f} {c2[0]*s:.1f} {c2[1]*s:.1f} {p2[0]*s:.1f} {p2[1]*s:.1f}')
    d.append('Z')
    return ''.join(d)

def layer(mask, eps=None, minarea=None):
    eps = EPS if eps is None else eps
    minarea = MINA if minarea is None else minarea
    ps = []
    for loop in contours(mask):
        # drop duplicate closing point
        pts = loop[:-1] if loop[0] == loop[-1] else loop
        simp = rdp_closed(pts, eps)
        if len(simp) < 4: continue
        a = 0.0
        for i in range(len(simp)):
            x1, y1 = simp[i]; x2, y2 = simp[(i+1) % len(simp)]
            a += x1*y2 - x2*y1
        if abs(a)/2 < minarea: continue
        ps.append(smooth_path(simp, SCALE))
    return ''.join(ps)

I = {n: names.index(n) for n in names}
sil   = layer(mask_of(lambda v, b: not b))
ink   = layer(mask_of(lambda v, b: v == I['INK']))
gray  = layer(mask_of(lambda v, b: v == I['GRAY']))
teal  = layer(mask_of(lambda v, b: v == I['TEAL']))
deep  = layer(mask_of(lambda v, b: v == I['DEEP']))

vw, vh = W*SCALE, H*SCALE
svg = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {vw:.0f} {vh:.0f}" width="{vw:.0f}" height="{vh:.0f}" role="img" aria-label="plugkill watchdog">
  <g fill-rule="evenodd">
    <path fill="#fdfdfd" d="{sil}"/>
    <path fill="#b6c4ca" d="{gray}"/>
    <path fill="#1a2429" d="{ink}"/>
    <path fill="#29cfd7" d="{teal}"/>
    <path fill="#109fa6" d="{deep}"/>
  </g>
</svg>
'''
open(OUT, 'w').write(svg)
print(f'{W}x{H} -> {OUT}  {len(svg)//1024} KB')
