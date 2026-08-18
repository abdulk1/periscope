#!/usr/bin/env python3
"""Draws Periscope's app icon.

A generator rather than a checked-in image, so the mark can be changed by
editing numbers instead of opening a design tool, and so the 1024px master and
every derived size come from the same source. Writes a PNG; `bundle-macos.sh`
turns it into an `.icns` with `iconutil`.

Deliberately dependency-free — no Pillow, no cairo — because a build step that
needs a Python environment set up is a build step that stops working. The PNG
encoder below is the whole of what those libraries would have been used for.
"""

from __future__ import annotations

import struct
import sys
import zlib
from pathlib import Path

SIZE = 1024

# macOS leaves a margin around the icon shape itself; 1024px art inside a 1024px
# canvas looks oversized next to every other icon in the Dock.
MARGIN = 100

BACKGROUND_TOP = (18, 32, 54)
BACKGROUND_BOTTOM = (11, 20, 36)
GLYPH = (125, 211, 252)
LENS = (250, 250, 250)


def rounded_square(x: float, y: float, size: int, margin: int, radius: float) -> bool:
    """Whether a point is inside the icon's rounded background."""
    left, top = margin, margin
    right, bottom = size - margin, size - margin
    if not (left <= x <= right and top <= y <= bottom):
        return False

    # Only the four corner boxes need the circle test.
    for corner_x, corner_y in (
        (left + radius, top + radius),
        (right - radius, top + radius),
        (left + radius, bottom - radius),
        (right - radius, bottom - radius),
    ):
        inside_x = (x < left + radius) if corner_x < size / 2 else (x > right - radius)
        inside_y = (y < top + radius) if corner_y < size / 2 else (y > bottom - radius)
        if inside_x and inside_y:
            return (x - corner_x) ** 2 + (y - corner_y) ** 2 <= radius**2
    return True


def periscope(x: float, y: float, size: int) -> str | None:
    """Which part of the periscope glyph a point belongs to, if any.

    The mark is the instrument: a vertical tube, a head turned to the right, and
    a lens at the end of it.
    """
    unit = size / 1024

    tube_left, tube_right = 430 * unit, 560 * unit
    tube_top, tube_bottom = 300 * unit, 760 * unit
    head_top, head_bottom = 300 * unit, 430 * unit
    head_right = 720 * unit

    # The lens, sitting proud of the head.
    lens_x, lens_y, lens_r = 690 * unit, 365 * unit, 46 * unit
    if (x - lens_x) ** 2 + (y - lens_y) ** 2 <= lens_r**2:
        return "lens"

    if tube_left <= x <= tube_right and tube_top <= y <= tube_bottom:
        return "glyph"
    if tube_left <= x <= head_right and head_top <= y <= head_bottom:
        return "glyph"

    # A foot, so the tube does not look like it was cut off.
    foot_left, foot_right = 380 * unit, 610 * unit
    if foot_left <= x <= foot_right and tube_bottom <= y <= tube_bottom + 70 * unit:
        return "glyph"

    return None


def pixels(size: int) -> bytes:
    """Renders the icon as raw RGBA scanlines, with a filter byte per row."""
    radius = size * 0.22
    margin = round(MARGIN * size / SIZE)
    rows = bytearray()

    for y in range(size):
        rows.append(0)  # PNG filter: none
        # A vertical gradient, so the background is not a flat slab.
        blend = y / max(size - 1, 1)
        background = tuple(
            round(top + (bottom - top) * blend)
            for top, bottom in zip(BACKGROUND_TOP, BACKGROUND_BOTTOM)
        )

        for x in range(size):
            point = (x + 0.5, y + 0.5)
            if not rounded_square(point[0], point[1], size, margin, radius):
                rows.extend((0, 0, 0, 0))
                continue

            part = periscope(point[0], point[1], size)
            if part == "lens":
                rows.extend((*LENS, 255))
            elif part == "glyph":
                rows.extend((*GLYPH, 255))
            else:
                rows.extend((*background, 255))

    return bytes(rows)


def chunk(kind: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    )


def png(size: int) -> bytes:
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(pixels(size), 9))
        + chunk(b"IEND", b"")
    )


def main() -> int:
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "AppIcon.png")
    out.write_bytes(png(SIZE))
    print(f"wrote {out} ({SIZE}x{SIZE})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
