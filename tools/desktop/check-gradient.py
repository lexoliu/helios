#!/usr/bin/env python3
"""Check that a scanout capture holds the gradient `display-test` draws.

The point of the check is that "the guest drew" and "the file is a PNG"
are different claims. A capture of a display nothing ever attached a
resource to is also a valid PNG; only the pixels say whether the frame
buffer reached the screen.

`display-test` fills its surface with a gradient whose colour at every
pixel is a function of that pixel's position: red rises to the right,
green rises downward, blue is the same everywhere. Both are computed
here from the capture's own dimensions, so the check needs to be told
nothing about the mode the guest chose.

Usage: check-gradient.py <capture.png> [more.png ...]
"""

from __future__ import annotations

import struct
import sys
import zlib
from pathlib import Path

PNG_MAGIC = b"\x89PNG\r\n\x1a\n"

# The constant blue channel `display-test` writes.
GRADIENT_BLUE = 0x40

# How far a channel may be from what the formula says.
#
# The path from the guest's frame buffer to the file is byte-for-byte —
# the display engine copies, it does not resample — so the tolerance is
# for a host that converts between pixel orderings and rounds, not for a
# picture that is merely gradient-like.
TOLERANCE = 4


class CaptureError(Exception):
    """The capture is not a picture this check can read."""


def read_png(path: Path) -> tuple[int, int, int, bytes]:
    """Return (width, height, channels, pixels) of a PNG.

    Only what QEMU's own writer emits is handled: 8 bits a channel, RGB
    or RGBA, no interlacing. Anything else is an error rather than a
    guess, because a guess here would report a picture that was never
    checked.
    """
    data = path.read_bytes()
    if not data.startswith(PNG_MAGIC):
        raise CaptureError(f"{path} does not start with the PNG signature")
    offset = len(PNG_MAGIC)
    width = height = channels = 0
    compressed = bytearray()
    while offset + 8 <= len(data):
        (length,) = struct.unpack_from(">I", data, offset)
        kind = data[offset + 4 : offset + 8]
        body = data[offset + 8 : offset + 8 + length]
        offset += 12 + length
        if kind == b"IHDR":
            width, height, depth, colour, compression, filt, interlace = struct.unpack(
                ">IIBBBBB", body
            )
            if depth != 8:
                raise CaptureError(f"{path} is {depth} bits a channel, not 8")
            if colour not in (2, 6):
                raise CaptureError(f"{path} has colour type {colour}, not RGB or RGBA")
            if compression != 0 or filt != 0 or interlace != 0:
                raise CaptureError(f"{path} is compressed or interlaced unusually")
            channels = 3 if colour == 2 else 4
        elif kind == b"IDAT":
            compressed += body
        elif kind == b"IEND":
            break
    if width == 0 or height == 0 or channels == 0:
        raise CaptureError(f"{path} carries no image header")
    return width, height, channels, unfilter(
        zlib.decompress(bytes(compressed)), width, height, channels, path
    )


def unfilter(raw: bytes, width: int, height: int, channels: int, path: Path) -> bytes:
    """Undo the per-scanline filters PNG applies (RFC 2083 §6)."""
    stride = width * channels
    out = bytearray(stride * height)
    previous = bytearray(stride)
    position = 0
    for row in range(height):
        if position >= len(raw):
            raise CaptureError(f"{path} ends after {row} of {height} rows")
        kind = raw[position]
        line = bytearray(raw[position + 1 : position + 1 + stride])
        position += 1 + stride
        for index in range(stride):
            left = line[index - channels] if index >= channels else 0
            up = previous[index]
            up_left = previous[index - channels] if index >= channels else 0
            value = line[index]
            if kind == 0:
                pass
            elif kind == 1:
                value += left
            elif kind == 2:
                value += up
            elif kind == 3:
                value += (left + up) // 2
            elif kind == 4:
                value += paeth(left, up, up_left)
            else:
                raise CaptureError(f"{path} row {row} uses filter {kind}")
            line[index] = value & 0xFF
        out[row * stride : (row + 1) * stride] = line
        previous = line
    return bytes(out)


def paeth(left: int, up: int, up_left: int) -> int:
    estimate = left + up - up_left
    distances = (abs(estimate - left), abs(estimate - up), abs(estimate - up_left))
    if distances[0] <= distances[1] and distances[0] <= distances[2]:
        return left
    if distances[1] <= distances[2]:
        return up
    return up_left


def expected(x: int, y: int, width: int, height: int) -> tuple[int, int, int]:
    """The colour `display-test` writes at this position."""
    return ((x * 255) // max(width, 1), (y * 255) // max(height, 1), GRADIENT_BLUE)


def check(path: Path) -> list[str]:
    width, height, channels, pixels = read_png(path)
    stride = width * channels
    # The corners and the middle: three of them pin both gradients at
    # both ends, and the middle catches a picture that is right at its
    # edges and wrong in between.
    samples = [
        (0, 0),
        (width - 1, 0),
        (0, height - 1),
        (width - 1, height - 1),
        (width // 2, height // 2),
    ]
    failures = []
    for x, y in samples:
        offset = y * stride + x * channels
        found = tuple(pixels[offset : offset + 3])
        want = expected(x, y, width, height)
        if any(abs(a - b) > TOLERANCE for a, b in zip(found, want)):
            failures.append(
                f"  at ({x}, {y}): found rgb{found}, expected about rgb{want}"
            )
    if failures:
        return [f"{path} ({width}x{height}) is not the gradient:"] + failures
    print(f"{path}: {width}x{height} gradient present at every sample")
    return []


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    problems = []
    for name in argv[1:]:
        try:
            problems += check(Path(name))
        except CaptureError as error:
            problems.append(str(error))
    for line in problems:
        print(line, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
