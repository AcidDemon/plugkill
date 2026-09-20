#!/usr/bin/env python3
"""Generate the bus icons for the dashboard tiles.

The sources are the stroked 16 by 16 line icons from the tray mockup. A
symbolic loader repaints fills only, so picosvg turns every stroke into a
filled outline, and each shape then gets the same ColorScheme-Text class and
currentColor fill as the tray icons from tray-icons.py.

picosvg must be importable. From the repository root:
  nix shell --impure --expr '(builtins.getFlake "nixpkgs").legacyPackages.${builtins.currentSystem}.python3.withPackages (p: [ p.picosvg ])' -c python3 assets/brand/bus-icons.py
(`nix shell nixpkgs#python3Packages.picosvg -c python3` does not put the
package on python3's path.)
Writes crates/plugkill-gui/icons/plugkill-bus-*-symbolic.svg
"""
import pathlib
import re
import sys
from typing import NoReturn

from picosvg.svg import SVG

HERE = pathlib.Path(__file__).resolve().parent
OUT = HERE.parent.parent / "crates" / "plugkill-gui" / "icons"

SOURCES = {
    "usb": '<path d="M8 1.6v10M8 9 11 7V5.2M8 10.2 5 8.2V6.6" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/><circle cx="8" cy="13" r="1.5" fill="currentColor"/><rect x="10" y="3.8" width="2" height="1.6" fill="currentColor"/><circle cx="5" cy="5.8" r="1" fill="currentColor"/>',
    "thunderbolt": '<path d="M9.2 1.8 4 9h4l-1 5.2L12.2 7H8.2Z" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/>',
    "sdcard": '<path d="M4 1.8h6.6l2 2v10.4H4Z M6.4 4.2v2M8.4 4.2v2M10.4 4.8v1.4" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" stroke-linecap="round"/>',
    "power": '<path d="M5.6 1.8V5M10.4 1.8V5M3.8 5h8.4v2.4c0 2.3-1.8 4-4.2 4s-4.2-1.7-4.2-4Z M8 11.4v2.8" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/>',
    "network": '<path d="M3 3.8h10v6.4h-3v2.6H6v-2.6H3Z M5.6 6.4v1.2M8 6.4v1.2M10.4 6.4v1.2" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" stroke-linecap="round"/>',
    "lid": '<path d="M3.4 3.6h9.2v6.8H3.4Z M1.6 12.6h12.8" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" stroke-linecap="round"/>',
    "pci": '<path d="M4.6 4.6h6.8v6.8H4.6Z M6.6 2v2.6M9.4 2v2.6M6.6 11.4V14M9.4 11.4V14M2 6.6h2.6M2 9.4h2.6M11.4 6.6H14M11.4 9.4H14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>',
    "display": '<path d="M2 2.8h12v7.8H2Z M5.8 13.6h4.4M8 10.6v3" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" stroke-linecap="round"/>',
}

# Same block as tray-icons.py.
STYLE = (
    '<style id="current-color-scheme" type="text/css">'
    ".ColorScheme-Text{color:#232629}"
    ".ColorScheme-NeutralText{color:#f67400}"
    ".ColorScheme-NegativeText{color:#da4453}"
    "</style>"
)
FG = 'class="ColorScheme-Text" fill="currentColor"'
# picosvg resolves paint to concrete colours, so convert in black and put
# currentColor back afterwards.
INK = "#000000"


def fail(message) -> NoReturn:
    sys.exit(f"bus-icons.py: {message}")


def convert(name, source):
    src = (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">'
        f'{source.replace("currentColor", INK)}</svg>'
    )
    paths = []
    for shape in SVG.fromstring(src).topicosvg().shapes():
        if shape.fill.lower() not in (INK, "#000", "black"):
            fail(f"{name}: unexpected fill {shape.fill!r}")
        rule = ' fill-rule="evenodd"' if shape.fill_rule == "evenodd" else ""
        paths.append(f'<path {FG}{rule} d="{shape.d}"/>')
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">'
        f"{STYLE}{''.join(paths)}</svg>\n"
    )


def check(name, svg):
    if STYLE not in svg or 'viewBox="0 0 16 16"' not in svg:
        fail(f"{name}: missing style block or 16 by 16 viewBox")
    if "stroke" in svg:
        fail(f"{name}: still draws with stroke")
    if re.search(r"#fff|white", svg, re.IGNORECASE):
        fail(f"{name}: has a white fill")
    shapes = re.findall(r"<(?!svg|style|/)(\w+)([^>]*)>", svg)
    if not shapes:
        fail(f"{name}: no shapes")
    for tag, attrs in shapes:
        if tag != "path" or not attrs.startswith(f" {FG}"):
            fail(f"{name}: <{tag}{attrs[:40]}> is not a ColorScheme-Text path")
        d = re.search(r'd="([^"]+)"', attrs)
        if not d:
            fail(f"{name}: path without a d attribute")
        numbers = [float(n) for n in re.findall(r"-?\d*\.?\d+(?:e-?\d+)?", d.group(1))]
        if not numbers or not all(0 <= n <= 16 for n in numbers):
            fail(f"{name}: path coordinates leave the 0..16 canvas")


OUT.mkdir(parents=True, exist_ok=True)
for name, source in SOURCES.items():
    svg = convert(name, source)
    check(name, svg)
    (OUT / f"plugkill-bus-{name}-symbolic.svg").write_text(svg)
print(f"wrote {len(SOURCES)} icons to {OUT}")
