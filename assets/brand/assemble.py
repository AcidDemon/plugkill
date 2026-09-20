#!/usr/bin/env python3
"""Assemble the final SVGs from the raw traced layers.

    assemble.py TMPDIR

Called by vectorize.sh, which produces TMPDIR/{icon,mascot,word}-raw.svg.

Three things happen here that the tracer deliberately leaves alone.

Ramp folding. The palettes name the anti-alias ramp between two design colours
so it cannot land on an unrelated entry. Those named ramps are not design
colours, so each is folded into the neighbour it belongs to: the dark half of
an ink/page ramp joins the ink, the light half is dropped with the page.

The cable is drawn, not traced. In the source art its body is the same ink as
the plate and only a white keyline made it visible, so traced on a dark plate
it reads as disconnected slivers. The icon gets a mid-slate cable under the
traced highlight; the banner gets a severed one, two sheared stubs with spark
ticks and the plug falling away, because a cut connection is what the daemon
actually does.

Layout comes from the art, not from constants. The mascot and the wordmark are
retraced whenever the concept art changes, and a crop that moves by ten pixels
moves everything that was positioned against the old one. So the wordmark is
placed from its measured ink extent and the viewBoxes are read back from the
traced files.

The tray glyph is a single path filled with currentColor, so tray.rs can tint
one shape for all four daemon states instead of drawing four coloured circles.
It comes from thresholding the finished icon rather than from the trace, so it
always matches whatever the icon currently is.
"""

import pathlib
import re
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent


def layers(path):
    return dict(re.findall(r'<path fill="(#[0-9a-f]{6})" d="([^"]*)"/>',
                           pathlib.Path(path).read_text()))


def inner(svg_text):
    return svg_text[svg_text.index('>', svg_text.index('<svg')) + 1:
                    svg_text.rindex('</svg>')]


def dims(path):
    """Width and height from a traced file, so nothing here hardcodes a size
    that a changed crop would silently clip."""
    m = re.search(r'viewBox="0 0 (\d+) (\d+)"', pathlib.Path(path).read_text())
    if not m:
        raise SystemExit(f'no viewBox in {path}')
    return int(m.group(1)), int(m.group(2))


def subpaths(d):
    return ['M' + s for s in d.split('M') if s.strip()]


def bbox(p):
    n = [float(x) for x in re.findall(r'-?\d+\.?\d*', p)]
    xs, ys = n[0::2], n[1::2]
    return min(xs), min(ys), max(xs), max(ys)


def boxarea(p):
    x0, y0, x1, y1 = bbox(p)
    return (x1 - x0) * (y1 - y0)


def platebar(p):
    """A long flat sliver, which on the icon is the plate's own anti-aliased
    rim caught by the white layer rather than any part of the goose. Left in,
    it draws a white hairline across the top of the plate."""
    x0, y0, x1, y1 = bbox(p)
    w, h = x1 - x0, y1 - y0
    return h < 40 and w > 300 and w > 8 * h


def build_icon(tmp):
    L = layers(tmp / 'icon-raw.svg')
    # The ink layer is the whole canvas and equals the plate, so the plate rect
    # stands in for it. That frees the slot beneath the white layer for a cable
    # body the white highlight can then sit on top of.
    cable = ('    <path d="M584 366 L612 438" stroke="#5d6e79" stroke-width="24" '
             'fill="none" stroke-linecap="round"/>\n')
    white = ''.join(p for p in subpaths(L['#f2f3f5']) if not platebar(p))
    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 760 760" width="760" height="760" role="img" aria-label="plugkill">
  <defs><clipPath id="art"><rect x="10" y="10" width="740" height="740" rx="158" ry="158"/></clipPath></defs>
  <rect x="0" y="0" width="760" height="760" rx="168" ry="168" fill="#162028"/>
  <g clip-path="url(#art)" fill-rule="evenodd">
{cable}    <path fill="#f2f3f5" d="{white}"/>
    <path fill="#01dfe0" d="{L['#01dfe0']}"/>
    <path fill="#fc990b" d="{L['#fc990b']}"/>
  </g>
</svg>
'''
    (HERE / 'icon.svg').write_text(svg)


def build_mascot(tmp):
    L = layers(tmp / 'mascot-raw.svg')
    w, h = dims(tmp / 'mascot-raw.svg')
    ink = L.get('#16212a', '') + L.get('#42555c', '')     # ramp folded into ink

    # The white layer is the set of enclosed page-coloured pockets, so a loop
    # inside another is a hole that shows the ink base through. Erasing the
    # breast badge leaves one 58 square pixel island behind, which reads as a
    # dark fleck on the plumage; the smallest pocket that belongs there, the
    # eye highlight, is 250, so anything under 120 is debris.
    white = ''.join(p for p in subpaths(L.get('#fdfdfd', '')) if boxarea(p) >= 120)

    body = [f'    <path fill="#16212a" d="{ink}"/>']
    # ORANGEEDGE is the light half of the ink/orange ramp and belongs to the beak
    orange = L.get('#fc990b', '') + L.get('#c27b13', '')
    for c, d in (('#fdfdfd', white), ('#03acb0', L.get('#03acb0', '')),
                 ('#fc990b', orange)):
        if d:
            body.append(f'    <path fill="{c}" d="{d}"/>')
    (HERE / 'mascot.svg').write_text(
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" '
        f'width="{w}" height="{h}" role="img" aria-label="plugkill guard goose">\n'
        '  <g fill-rule="evenodd">\n' + '\n'.join(body) + '\n  </g>\n</svg>\n')


def build_wordmark(tmp):
    L = layers(tmp / 'word-raw.svg')
    w, h = dims(tmp / 'word-raw.svg')
    # DARKEDGE is the ink side of the ink/page ramp and joins the ink. LIGHTEDGE
    # is the page side and goes with the page. Left on their own they render as
    # teal hairlines down every dark letter, because the teal sits near the
    # midpoint of that ramp in RGB.
    ink = L.get('#19242b', '') + L.get('#747b84', '')
    (HERE / 'wordmark.svg').write_text(
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" '
        f'width="{w}" height="{h}" role="img" aria-label="plugkill">\n'
        '  <g fill-rule="evenodd">\n'
        f'    <path fill="#19242b" d="{ink}"/>\n'
        f'    <path fill="#04a6ac" d="{L.get("#04a6ac", "")}"/>\n'
        '  </g>\n</svg>\n')


def build_banners():
    mascot_svg = (HERE / 'mascot.svg').read_text()
    mascot = inner(mascot_svg)
    word_svg = (HERE / 'wordmark.svg').read_text()
    word = inner(word_svg)
    # Mascot is 900x853 with its beak tip at (696,149).
    ms, mx, my = 0.52, 50, 48
    bx, by = mx + 696 * ms, my + 149 * ms
    # The cord is severed at the beak and nothing is drawn past it. Two
    # earlier versions kept a held stub out beyond the tip; a stub reads as a
    # cable lying across the beak, and wherever the shear then went it looked
    # arbitrary, because the break was not where the bite was. So the beak is
    # the break: the falling piece starts 12 pixels off the tip with its
    # sheared end square to the cord, and the sparks sit on that break.
    CW = 11
    fx0, fy0 = bx + 9, by + 8             # sheared end, right off the beak tip
    fx1, fy1 = bx + 29, by + 45           # where the plug begins

    # USB-A plug, measured off Concept 2 of the goose art (which is not in the
    # repo, see vectorize.sh), in units of the cord width so that changing CW
    # rescales the whole plug.
    #
    # Concept 2 draws the plug differently from Concept 1, and it is Concept 2
    # that reads. The difference is one thing: its shell is a pale window in a
    # thin dark frame, 72 percent of the shell's width and 88 percent of its
    # length, so the tip reads as an opening. Every earlier attempt made the
    # shell mostly dark with a small pale panel set into it, which reads as a
    # label on a solid object, and no amount of adjusting the panel fixes that,
    # because the dark around it is what the eye is reading.
    #
    # The rest is proportion, also from Concept 2: a neck about half the
    # overmold's width, then the overmold at full width, then the shell
    # stepping in slightly. The step is what says "connector" before the window
    # is legible, and it is why the shell must not be as wide as the overmold.
    PLUG = [
        # fill,  x0,  y0,   x1,  y1, corner radius
        ('body',  -7, -13,  30,  13, 7),    # neck out of the cord
        ('body',  26, -25,  72,  25, 13),   # overmold, widest and roundest
        ('body',  68, -22, 102,  22, 3),    # shell, steps in, hard cornered
        ('face',  72, -16,  97,  16, 0),    # the window, and the whole read
        ('body',  80, -12,  88,  -4, 0),    # contacts
        ('body',  80,   4,  88,  12, 0),
    ]

    def cable(body, face):
        """The severed cord and its plug, in two colours so it survives a dark
        ground. On #0d1117 an ink cord is invisible, and the cut is the whole
        mark, so the dark lockup inverts the plug's contrast."""
        u = CW / 18.0
        fill = {'body': body, 'face': face}
        parts = '\n'.join(
            f'    <rect x="{x0 * u:.1f}" y="{y0 * u:.1f}" '
            f'width="{(x1 - x0) * u:.1f}" height="{(y1 - y0) * u:.1f}" '
            f'rx="{r * u:.1f}" fill="{fill[k]}"/>'
            for k, x0, y0, x1, y1, r in PLUG)
        return f'''
  <path d="M{fx0:.0f} {fy0:.0f} L{fx1:.0f} {fy1:.0f}" stroke="{body}" stroke-width="{CW}" fill="none" stroke-linecap="butt"/>
  <g transform="translate({fx0:.0f},{fy0:.0f})" fill="#03acb0">
    <rect x="12" y="-4" width="22" height="8" rx="4" transform="rotate(-70)"/>
    <rect x="12" y="-4" width="22" height="8" rx="4" transform="rotate(-40)"/>
    <rect x="12" y="-4" width="22" height="8" rx="4" transform="rotate(-10)"/>
  </g>
  <g transform="translate({fx1:.0f},{fy1:.0f}) rotate(62)">
{parts}
  </g>
'''

    # Place the wordmark from its ink extent rather than from its viewBox. The
    # traced file carries whatever padding the crop happened to leave, so a
    # fixed scale and offset drift every time the crop moves; the last pair
    # pushed the final l past the right edge and dropped the tagline into the
    # descenders.
    WX, WY, WW = 596, 116, 796
    x0, y0, x1, y1 = bbox(''.join(re.findall(r' d="([^"]*)"', word_svg)))
    s = WW / (x1 - x0)
    wpos = f'translate({WX - x0 * s:.1f},{WY - y0 * s:.1f}) scale({s:.4f})'
    tagy = WY + (y1 - y0) * s + 54
    # On a dark ground an ink outline is invisible, which flattens the goose
    # into a white paper cut and loses every feather line, so the dark lockup
    # lifts the outline to a slate that reads against both the plumage and the
    # page.
    variants = (
        ('banner-light', '#04a6ac', '#19242b', '#16212a',
         ('#16212a', '#fdfdfd')),
        ('banner-dark', '#03acb0', '#f0f6fc', '#46596a',
         ('#dfe7ec', '#16212a')),
    )
    for name, tagfill, wordink, maskink, wire in variants:
        w = word.replace('#19242b', wordink)
        m = mascot.replace('#16212a', maskink)
        (HERE / f'{name}.svg').write_text(
            f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1440 520" width="1440" height="520" role="img" aria-label="plugkill">
  <g transform="translate({mx},{my}) scale({ms})">{m}</g>
{cable(*wire)}
  <g transform="{wpos}">{w}</g>
  <text x="{WX}" y="{tagy:.0f}" textLength="{WW}" lengthAdjust="spacing" font-family="DejaVu Sans Mono, GeistMono Nerd Font, monospace" font-size="26" fill="{tagfill}">SUSPICIOUS CONNECTIONS STOP HERE.</text>
</svg>
''')


def build_glyph(tmp):
    """Threshold the finished icon into one tintable path for the tray."""
    png, two = tmp / 'icon760.png', tmp / 'glyph-2col.png'
    subprocess.run(['resvg', '-w', '760', str(HERE / 'icon.svg'), str(png)], check=True)
    subprocess.run(['magick', str(png), '-background', '#162028', '-flatten',
                    '-colorspace', 'gray', '-threshold', '40%',
                    '+level-colors', '#fcfdfd,#162028', str(two)], check=True)
    subprocess.run([sys.executable, str(HERE / 'trace.py'), str(two),
                    str(tmp / 'glyph-raw.svg'), 'BG:fcfdfd,INK:162028',
                    '--base', 'none', '--median', '3', '--eps', '0.9',
                    '--minarea', '22'], check=True)
    ink = layers(tmp / 'glyph-raw.svg')['#162028']
    kept = [p for p in subpaths(ink) if not platebar(p)]
    xs, ys = [], []
    for p in kept:
        x0, y0, x1, y1 = bbox(p)
        xs += [x0, x1]
        ys += [y0, y1]
    m = 8
    vx, vy = min(xs) - m, min(ys) - m
    vw, vh = max(xs) - vx + m, max(ys) - vy + m
    (HERE / 'glyph-mono.svg').write_text(
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vx:.0f} {vy:.0f} {vw:.0f} {vh:.0f}" '
        f'width="{vw:.0f}" height="{vh:.0f}" role="img" aria-label="plugkill">\n'
        f'  <path fill="currentColor" fill-rule="evenodd" d="{"".join(kept)}"/>\n</svg>\n')


def main():
    tmp = pathlib.Path(sys.argv[1])
    build_icon(tmp)
    build_mascot(tmp)
    build_wordmark(tmp)
    build_banners()
    build_glyph(tmp)


if __name__ == '__main__':
    main()
