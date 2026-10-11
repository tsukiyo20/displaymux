#!/usr/bin/env python3
"""Generate the displaymux application icon.

The mark is a multiplexer drawn as a circuit trace: two inputs converge into
one output. The live branch is amber, the dormant branch grey -- the same
distinction the app itself makes between "the input you last sent here" and
every other input. The stance is deliberately asymmetric so the mark does not
read as a letter.

This script is the single source of truth for every icon asset in `assets/`.
Run it after changing the geometry:

    python3 tools/make-icon.py

External tools (all optional except a rasteriser):
  * resvg / rsvg-convert / cairosvg -- one of these is required for PNG output
  * iconutil                       -- macOS only, produces icon.icns
  * Pillow (PIL)                   -- produces icon.ico

Everything is generated; nothing here is hand-drawn, so the assets can be
re-derived from this file alone.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys

# --------------------------------------------------------------- geometry ---
# All coordinates are offsets from the centre of a 1024x1024 canvas. The
# squircle occupies half-extent 412, i.e. the 824pt box Apple's macOS icon
# grid specifies.

CANVAS = 1024.0
CENTRE = CANVAS / 2
HALF = 412.0
STROKE = 92.0

# Live branch: full length, ends in the stem that leaves for the display.
LIVE = [(-150, -330), (-150, -96), (0, 80), (0, 330)]
# Dormant branch: a shorter stub, which is what stops the mark reading as "Y".
DORMANT = [(150, -250), (150, -96), (0, 80)]

CHASSIS = "#1B2027"
ACCENT = "#FFB627"
GHOST = "#73808F"

# The dormant branch's share of full opacity, used by the monochrome menu bar
# glyph where colour cannot carry the distinction.
DORMANT_ALPHA = 0.45


def trace_path(points) -> str:
    """SVG path for a polyline through centre-relative points."""
    (x0, y0), *rest = points
    parts = [f"M {CENTRE + x0:.1f} {CENTRE + y0:.1f}"]
    parts += [f"L {CENTRE + x:.1f} {CENTRE + y:.1f}" for x, y in rest]
    return " ".join(parts)


def squircle(cx: float, cy: float, r: float, n: float = 5.0, steps: int = 256) -> str:
    """Superellipse path. n=5 tracks the macOS Big Sur icon shape closely."""
    import math

    pts = []
    for i in range(steps):
        t = 2 * math.pi * i / steps
        ct, st = math.cos(t), math.sin(t)
        x = cx + r * math.copysign(abs(ct) ** (2.0 / n), ct)
        y = cy + r * math.copysign(abs(st) ** (2.0 / n), st)
        pts.append(f"{x:.2f},{y:.2f}")
    return "M " + " L ".join(pts) + " Z"


def app_icon_svg(shadow: bool = False) -> str:
    """The full colour application icon.

    No drop shadow by default: macOS composites its own, and baking one in
    produces a doubled edge in the Dock.
    """
    filters = f'''
    <filter id="drop" x="-25%" y="-25%" width="150%" height="150%">
      <feDropShadow dx="0" dy="16" stdDeviation="20" flood-color="#000" flood-opacity="0.30"/>
    </filter>''' if shadow else ""
    body = f'''
  <g{(' filter="url(#drop)"' if shadow else '')}>
    <path d="{squircle(CENTRE, CENTRE, HALF)}" fill="{CHASSIS}"/>
  </g>'''
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
  <title>displaymux</title>
  <defs>
    <linearGradient id="rim" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0"    stop-color="#FFFFFF" stop-opacity="0.20"/>
      <stop offset="0.55" stop-color="#FFFFFF" stop-opacity="0.03"/>
      <stop offset="1"    stop-color="#FFFFFF" stop-opacity="0.00"/>
    </linearGradient>{filters}
  </defs>{body}
  <path d="{squircle(CENTRE, CENTRE, HALF)}" fill="none" stroke="url(#rim)" stroke-width="3"/>
  <path d="{trace_path(DORMANT)}" fill="none" stroke="{GHOST}" stroke-width="{STROKE}"
        stroke-linecap="round" stroke-linejoin="round"/>
  <path d="{trace_path(LIVE)}" fill="none" stroke="{ACCENT}" stroke-width="{STROKE}"
        stroke-linecap="round" stroke-linejoin="round"/>
</svg>
'''


def mark_svg(opacity: bool = True) -> str:
    """Glyph only, no chassis -- for the README and as a tray reference."""
    dim = f' stroke-opacity="{DORMANT_ALPHA}"' if opacity else ""
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
  <title>displaymux mark</title>
  <path d="{trace_path(DORMANT)}" fill="none" stroke="{GHOST}" stroke-width="{STROKE}"
        stroke-linecap="round" stroke-linejoin="round"{dim}/>
  <path d="{trace_path(LIVE)}" fill="none" stroke="{ACCENT}" stroke-width="{STROKE}"
        stroke-linecap="round" stroke-linejoin="round"/>
</svg>
'''


# -------------------------------------------------------------- rasterising ---

def find_rasteriser():
    for name, argv in (
        ("resvg", lambda s, n, o: ["resvg", "-w", str(n), s, o]),
        ("rsvg-convert", lambda s, n, o: ["rsvg-convert", "-w", str(n), "-o", o, s]),
    ):
        if shutil.which(name):
            return name, argv
    try:
        import cairosvg  # noqa: F401
        return "cairosvg", None
    except ImportError:
        return None, None


def rasterise(svg_path: str, size: int, out_path: str, tool, argv) -> bool:
    if tool == "cairosvg":
        import cairosvg
        cairosvg.svg2png(url=svg_path, write_to=out_path,
                         output_width=size, output_height=size)
        return True
    proc = subprocess.run(argv(svg_path, size, out_path),
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr.decode(errors="replace"))
    return proc.returncode == 0


def main() -> int:
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    assets = os.path.join(root, "assets")
    os.makedirs(assets, exist_ok=True)

    svg_path = os.path.join(assets, "icon.svg")
    with open(svg_path, "w") as fh:
        fh.write(app_icon_svg())
    with open(os.path.join(assets, "mark.svg"), "w") as fh:
        fh.write(mark_svg())
    print(f"wrote {os.path.relpath(svg_path, root)}")

    tool, argv = find_rasteriser()
    if tool is None:
        print("no SVG rasteriser found (resvg / rsvg-convert / cairosvg); "
              "SVG written, PNG/ICNS/ICO skipped", file=sys.stderr)
        return 1
    print(f"rasteriser: {tool}")

    png_sizes = (16, 24, 32, 48, 64, 128, 256, 512, 1024)
    for size in png_sizes:
        out = os.path.join(assets, f"icon-{size}.png")
        if rasterise(svg_path, size, out, tool, argv):
            print(f"  icon-{size}.png")

    # macOS .icns via an intermediate .iconset directory.
    if shutil.which("iconutil"):
        iconset = os.path.join(assets, "icon.iconset")
        os.makedirs(iconset, exist_ok=True)
        for pt in (16, 32, 128, 256, 512):
            for scale, suffix in ((1, ""), (2, "@2x")):
                px = pt * scale
                dest = os.path.join(iconset, f"icon_{pt}x{pt}{suffix}.png")
                rasterise(svg_path, px, dest, tool, argv)
        icns = os.path.join(assets, "icon.icns")
        proc = subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns],
                              stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        if proc.returncode == 0:
            print("  icon.icns")
        else:
            sys.stderr.write(proc.stderr.decode(errors="replace"))
        shutil.rmtree(iconset, ignore_errors=True)
    else:
        print("iconutil not found (macOS only); icon.icns skipped", file=sys.stderr)

    # Windows .ico needs Pillow to pack multiple sizes into one file. Pillow
    # downsamples from a single source image, so hand it the largest frame.
    try:
        from PIL import Image
        ico_sizes = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64),
                     (128, 128), (256, 256)]
        base = Image.open(os.path.join(assets, "icon-256.png")).convert("RGBA")
        base.save(os.path.join(assets, "icon.ico"), format="ICO", sizes=ico_sizes)
        print("  icon.ico")
    except ImportError:
        print("Pillow not found; icon.ico skipped", file=sys.stderr)
    except FileNotFoundError as exc:
        print(f"icon.ico skipped: {exc}", file=sys.stderr)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
