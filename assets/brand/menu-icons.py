#!/usr/bin/env python3
"""Render the right-click menu icons as 16 by 16 PNGs.

The panel draws the menu itself and knows nothing about our IconThemePath, so
a theme name would show nothing on most desktops. DBusMenu carries icon bytes
instead, and tray.rs embeds these files. Most icons are one neutral grey that
reads on a dark menu and on a light one; the few that carry meaning are tinted
with the state colours below, because the host lets us colour nothing else in
the menu.

Needs resvg, which the script calls through nix. From the repository root:
  python3 assets/brand/menu-icons.py
Writes crates/plugkill-gui/icons/png/<name>.png
"""
import pathlib
import struct
import subprocess
import sys
import tempfile
from typing import NoReturn

HERE = pathlib.Path(__file__).resolve().parent
ICONS = HERE.parent.parent / "crates" / "plugkill-gui" / "icons"
OUT = ICONS / "png"
# The one tone that clears 3:1 both ways: 3.7:1 on a white menu, 3.6:1 on a
# #2d2d2d one. DBusMenu carries raster bytes, so the panel cannot recolour it.
GREY = "#7d858c"
# The state colours, the dashboard's hues at the luminance a 1.3 px stroke
# needs. The dashboard uses them as pill backgrounds behind dark text, so its
# lighter values are wrong here and this palette stays separate from
# dashboard/style.css on purpose. White then #2d2d2d, same as GREY above:
# armed 4.0 and 3.4, learn 4.0 and 3.5, disarmed 4.1 and 3.4.
ARMED = "#009411"
LEARN = "#9e7b00"
DISARMED = "#c96000"
SIZE = 16
# The corner dot that says watched. The glyph is masked a little wider than
# the dot so the two never touch at 16 px.
DOT_X = DOT_Y = 12.4
DOT_R = 2.9

BUSES = ["usb", "thunderbolt", "sdcard", "power", "network", "lid", "pci", "display"]
STATES = ["armed", "learning", "disarmed", "pending", "down"]

# The menu's own verbs. Inner markup of a 16 by 16 canvas; the bus and state
# icons come from the SVGs next to them.
ACTIONS = {
    "dashboard": '<path d="M2 2.6h5.2v4.6H2Z M8.8 2.6H14v7.4H8.8Z M2 8.8h5.2v4.6H2Z M8.8 11.6H14v1.8H8.8Z" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>',
    "clock": '<circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M8 4.4V8l2.6 1.8" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>',
    "shield": '<path d="M8 1.8 13 3.6v4.2c0 3.2-2.1 5.4-5 6.4-2.9-1-5-3.2-5-6.4V3.6Z" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/>',
    "eye": '<path d="M1.6 8S4 3.8 8 3.8 14.4 8 14.4 8 12 12.2 8 12.2 1.6 8 1.6 8Z" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><circle cx="8" cy="8" r="2" fill="none" stroke="currentColor" stroke-width="1.3"/>',
    "refresh": '<path d="M13 8a5 5 0 1 1-1.6-3.7" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><path d="M13.2 1.9v3.1h-3.1" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/>',
    "quit": '<path d="M8 2v6.4" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/><path d="M4.6 4.4a6 6 0 1 0 6.8 0" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>',
    "maintenance": '<circle cx="3.2" cy="8" r="1.5" fill="currentColor"/><circle cx="8" cy="8" r="1.5" fill="currentColor"/><circle cx="12.8" cy="8" r="1.5" fill="currentColor"/>',
}

# The two items whose meaning moves with the state: the mode item wears the
# live mode, the arm and extend items wear the disarmed orange.
TINTED = {
    "shield-enforce": ("shield", ARMED),
    "shield-learn": ("shield", LEARN),
    "shield-disarmed": ("shield", DISARMED),
    "clock-disarmed": ("clock", DISARMED),
}


def fail(message) -> NoReturn:
    sys.exit(f"menu-icons.py: {message}")


def canvas(inner):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">{inner}</svg>'


def sources():
    """Every icon name with its SVG text, its colour and its corner dot."""
    for name in BUSES:
        svg = read(ICONS / f"plugkill-bus-{name}-symbolic.svg")
        yield f"{name}-on", svg, GREY, ARMED
        # Unwatched is the absence of the dot, not a second tone: a dimmer grey
        # out-contrasts GREY on a light panel, which inverts what it means.
        yield f"{name}-off", svg, GREY, None
    for name in STATES:
        yield name, read(ICONS / f"plugkill-{name}-symbolic.svg"), GREY, None
    for name, inner in ACTIONS.items():
        yield name, canvas(inner), GREY, None
    for name, (base, colour) in TINTED.items():
        yield name, canvas(ACTIONS[base]), colour, None


def read(path):
    if not path.is_file():
        fail(f"missing source {path}")
    return path.read_text()


def compose(svg, colour, dot):
    """The source in one colour, with the dot punched out of the glyph."""
    svg = svg.replace("currentColor", colour)
    if dot is None:
        return svg
    inner = svg[svg.index(">", svg.index("<svg")) + 1 : svg.rindex("</svg>")]
    return canvas(
        f'<mask id="dot"><rect width="16" height="16" fill="#fff"/>'
        f'<circle cx="{DOT_X}" cy="{DOT_Y}" r="{DOT_R + 1.1}" fill="#000"/></mask>'
        f'<g mask="url(#dot)">{inner}</g>'
        f'<circle cx="{DOT_X}" cy="{DOT_Y}" r="{DOT_R}" fill="{dot}"/>'
    )


def render(name, svg, tmp):
    src = tmp / f"{name}.svg"
    src.write_text(svg)
    out = OUT / f"{name}.png"
    done = subprocess.run(
        # fmt: off
        ["nix", "shell", "nixpkgs#resvg", "-c", "resvg",
         str(src), str(out), "-w", str(SIZE), "-h", str(SIZE)],
        # fmt: on
        capture_output=True,
        text=True,
    )
    if done.returncode:
        fail(f"{name}: resvg failed: {done.stderr.strip() or done.returncode}")
    check(name, out)


def check(name, path):
    data = path.read_bytes() if path.is_file() else b""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        fail(f"{name}: not a PNG ({len(data)} bytes)")
    width, height = struct.unpack(">II", data[16:24])
    if (width, height) != (SIZE, SIZE):
        fail(f"{name}: {width} by {height}, not {SIZE} by {SIZE}")
    # An all-transparent 16 by 16 PNG is about 70 bytes, a drawn one far more.
    if len(data) < 150:
        fail(f"{name}: {len(data)} bytes, drew nothing")


OUT.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory() as tmpdir:
    names = []
    for icon_name, icon_svg, icon_colour, icon_dot in sources():
        render(icon_name, compose(icon_svg, icon_colour, icon_dot), pathlib.Path(tmpdir))
        names.append(icon_name)
print(f"wrote {len(names)} icons to {OUT}")
