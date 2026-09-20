#!/usr/bin/env python3
"""Generate the plugkill-gui tray icons from glyph-mono.svg.

Every icon is a symbolic SVG on a 22 by 22 canvas. The shape carries the
state; the desktop supplies the colour. GTK hosts (waybar, the GNOME
AppIndicator extension, the dashboard) paint unclassed shapes in the
foreground colour and the "warning" and "error" classes in theme colours.
Plasma reads the ColorScheme-* classes. A symbolic loader repaints every
fill, so white details are even-odd holes and nothing uses stroke.

picosvg must be importable, for turning the goose outline into a fill. From
the repository root:
  nix shell --impure --expr '(builtins.getFlake "nixpkgs").legacyPackages.${builtins.currentSystem}.python3.withPackages (p: [ p.picosvg ])' -c python3 assets/brand/tray-icons.py
Writes ../../crates/plugkill-gui/icons/*.svg
"""
import pathlib
import re

from picosvg.svg import SVG

HERE = pathlib.Path(__file__).resolve().parent
OUT = HERE.parent.parent / "crates" / "plugkill-gui" / "icons"

# glyph-mono.svg's viewBox is "69 101 659 588"; its path uses only absolute
# M, C and Z commands, so every number pair is an x, y coordinate.
VB_X, VB_Y, VB_W = 69.0, 101.0, 659.0
_match = re.search(r'\sd="([^"]+)"', (HERE / "glyph-mono.svg").read_text())
assert _match, "no path d attribute in glyph-mono.svg"
GLYPH = _match.group(1)
_commands = set(re.findall(r"[A-Za-z]", GLYPH))
assert _commands <= {"M", "C", "Z"}, (
    f"glyph path uses {sorted(_commands - {'M', 'C', 'Z'})}; the transform "
    "only handles absolute M, C and Z"
)
PAIR = re.compile(r"(-?\d+(?:\.\d+)?)[ ,](-?\d+(?:\.\d+)?)")
COORD = re.compile(r"-?\d+(?:\.\d+)?")

# The drawn art does not fill the viewBox, so measure it and centre on that.
_xs = [float(a) for a, _ in PAIR.findall(GLYPH)]
_ys = [float(b) for _, b in PAIR.findall(GLYPH)]
ART_X, ART_Y = min(_xs) - VB_X, min(_ys) - VB_Y
ART_W, ART_H = max(_xs) - min(_xs), max(_ys) - min(_ys)

STYLE = (
    '<style id="current-color-scheme" type="text/css">'
    ".ColorScheme-Text{color:#232629}"
    ".ColorScheme-NeutralText{color:#f67400}"
    ".ColorScheme-NegativeText{color:#da4453}"
    "</style>"
)
FG = 'class="ColorScheme-Text" fill="currentColor"'
WARN = 'class="warning ColorScheme-NeutralText" fill="currentColor"'
ERR = 'class="error ColorScheme-NegativeText" fill="currentColor"'


def goose(x, y, width):
    """The glyph path moved to (x, y) and scaled to `width`."""
    s = width / VB_W

    def move(m):
        px, py = float(m.group(1)), float(m.group(2))
        return f"{(px - VB_X) * s + x:.2f} {(py - VB_Y) * s + y:.2f}"

    out = PAIR.sub(move, GLYPH)
    assert len(re.findall(r"-?\d+(?:\.\d+)?", out)) == len(
        re.findall(r"-?\d+(?:\.\d+)?", GLYPH)
    ), "coordinate count changed while transforming the glyph"
    return out


def centred(art_width):
    """The glyph with its drawn art `art_width` wide and centred on the canvas."""
    s = art_width / ART_W
    return goose(11 - (ART_W / 2 + ART_X) * s, 11 - (ART_H / 2 + ART_Y) * s, VB_W * s)


# Panels draw tray icons in a small box (waybar defaults to 16 px, and the
# author's bar asks for 14), and the goose is wider than it is tall, so it can
# never fill that box the way a square glyph does. Thickening the silhouette by
# its own outline buys back the visual weight that the height cannot.
OUTLINE = 0.7


def thicken(path, weight=OUTLINE):
    """The path's outline, stroked and turned into a fill picosvg-style."""
    stroked = (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22">'
        f'<path d="{path}" fill="none" stroke="#000" stroke-width="{weight}"'
        ' stroke-linejoin="round"/></svg>'
    )
    outline = SVG.fromstring(stroked).topicosvg().tostring()
    return "".join(re.findall(r'\sd="([^"]+)"', outline))


def rrect(x, y, w, h, r):
    return (
        f"M{x + r} {y}H{x + w - r}A{r} {r} 0 0 1 {x + w} {y + r}"
        f"V{y + h - r}A{r} {r} 0 0 1 {x + w - r} {y + h}"
        f"H{x + r}A{r} {r} 0 0 1 {x} {y + h - r}"
        f"V{y + r}A{r} {r} 0 0 1 {x + r} {y}Z"
    )


def circle(cx, cy, r):
    return f"M{cx - r} {cy}A{r} {r} 0 1 0 {cx + r} {cy}A{r} {r} 0 1 0 {cx - r} {cy}Z"


def svg(body, defs=""):
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="22" height="22" viewBox="0 0 22 22">'
        f"{STYLE}{defs}{body}</svg>\n"
    )


# The goose fills the canvas with about half a unit of margin; the cut-out one
# fits inside the rounded block with a margin of its own.
BIG = centred(21)
SMALL = centred(16.2)
BIG_EDGE = thicken(BIG)


def goose_with_gap(gap):
    """The big goose, clipped away from a badge so the badge stands apart."""
    defs = (
        '<defs><clipPath id="gap">'
        f'<path clip-rule="evenodd" d="M0 0H22V22H0Z{gap}"/>'
        "</clipPath></defs>"
    )
    return defs, (
        f'<path {FG} fill-rule="evenodd" clip-path="url(#gap)" d="{BIG}"/>'
        f'<path {FG} clip-path="url(#gap)" d="{BIG_EDGE}"/>'
    )


def armed():
    return svg(
        f'<path {FG} fill-rule="evenodd" d="{BIG}"/>'
        f'<path {FG} d="{BIG_EDGE}"/>'
    )


def learning():
    # Pushed into the corner as far as the 1.2 unit gap ring allows.
    cx = cy = 16.2
    defs, body = goose_with_gap(circle(cx, cy, 5.8))
    ring = circle(cx, cy, 4.6) + circle(cx, cy, 2.25)
    return svg(body + f'<path {WARN} fill-rule="evenodd" d="{ring}"/>', defs)


def disarmed():
    defs, body = goose_with_gap(rrect(10.0, 10.0, 12.0, 12.0, 3.8))
    badge = (
        rrect(11.2, 11.2, 9.6, 9.6, 2.6)
        + rrect(13.88, 13.5, 1.55, 5.0, 0.78)
        + rrect(16.57, 13.5, 1.55, 5.0, 0.78)
    )
    return svg(body + f'<path {WARN} fill-rule="evenodd" d="{badge}"/>', defs)


def pending():
    block = rrect(0.6, 0.6, 20.8, 20.8, 5) + SMALL
    return svg(f'<path {ERR} fill-rule="evenodd" d="{block}"/>')


def pending_blink():
    frame = rrect(0.6, 0.6, 20.8, 20.8, 5) + rrect(2.0, 2.0, 18.0, 18.0, 3.8)
    return svg(
        f'<path {ERR} fill-rule="evenodd" d="{frame}"/>'
        f'<path {ERR} fill-rule="evenodd" d="{SMALL}"/>'
    )


def down():
    slash = "M1.10 20.50L20.50 1.10L21.80 2.40L2.40 21.80Z"
    return svg(
        f'<path {FG} fill-rule="evenodd" opacity="0.35" d="{BIG}"/>'
        f'<path {FG} opacity="0.35" d="{BIG_EDGE}"/>'
        f'<path {FG} d="{slash}"/>'
    )


ICONS = {
    "armed": armed,
    "learning": learning,
    "disarmed": disarmed,
    "pending": pending,
    "pending-blink": pending_blink,
    "down": down,
}

def check_on_canvas(state, markup):
    """Every number in every path, clip paths included, stays within 0..22."""
    for d in re.findall(r'\sd="([^"]+)"', markup):
        off = [n for n in COORD.findall(d) if not 0.0 <= float(n) <= 22.0]
        assert not off, f"{state}: {len(off)} coordinates off the canvas: {off[:6]}"


OUT.mkdir(parents=True, exist_ok=True)
for state, make in ICONS.items():
    markup = make()
    check_on_canvas(state, markup)
    (OUT / f"plugkill-{state}-symbolic.svg").write_text(markup)
print(f"wrote {len(ICONS)} icons to {OUT}")
