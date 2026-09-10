#!/usr/bin/env python3
"""Check that the pointer `display-test` moved reached the display engine.

A scanout capture cannot show this. QEMU hands a virtio-gpu cursor to
its display frontend as a separate plane, and `screendump` reads the
scanout surface alone, so a capture of a guest driving its cursor looks
exactly like one that never set it. What can show it is the device's own
account: with `-d trace:virtio_gpu_update_cursor` QEMU logs every
`UPDATE_CURSOR` and `MOVE_CURSOR` it processes, with the position each
one carried.

`display-test` prints the position it moved the cursor to on every
frame it presents. The check is that each of those positions is one the
device logged as a move, and that the device saw the cursor image
(`UPDATE_CURSOR`, logged as `update` with a non-zero resource) before
any of them — which is what "the cursor is at the injected coordinates"
means for a device with no window.

Usage: check-cursor.py <inspector.log> <qemu-trace.log>
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# `display-test:frame sequence=<n> cursor=<x>,<y>`: the guest's own
# account of where it put the pointer when it presented that frame.
GUEST_FRAME = re.compile(r"display-test:frame sequence=(\d+) cursor=(\d+),(\d+)")

# QEMU's `virtio_gpu_update_cursor` trace event, as `hw/display/trace-events`
# spells it: `scanout %d, x %d, y %d, %s, res 0x%x`, where the word is
# `update` for `UPDATE_CURSOR` and `move` for `MOVE_CURSOR`. The log
# backend may prefix a line with a pid and a timestamp; only the event
# itself is matched.
DEVICE_CURSOR = re.compile(
    r"virtio_gpu_update_cursor scanout (\d+), x (\d+), y (\d+), (update|move), res 0x([0-9a-f]+)"
)


class CheckError(Exception):
    """A log this check cannot read."""


def read_lines(path: Path) -> list[str]:
    try:
        return path.read_text(errors="replace").splitlines()
    except OSError as error:
        raise CheckError(f"{path}: {error}") from error


def guest_positions(lines: list[str]) -> list[tuple[int, int, int]]:
    positions = []
    for line in lines:
        match = GUEST_FRAME.search(line)
        if match:
            sequence, x, y = (int(group) for group in match.groups())
            positions.append((sequence, x, y))
    return positions


def device_events(lines: list[str]) -> list[tuple[int, int, int, str, int]]:
    events = []
    for line in lines:
        match = DEVICE_CURSOR.search(line)
        if match:
            scanout, x, y, kind, resource = match.groups()
            events.append((int(scanout), int(x), int(y), kind, int(resource, 16)))
    return events


def check(inspector_log: Path, trace_log: Path) -> list[str]:
    problems = []
    frames = guest_positions(read_lines(inspector_log))
    events = device_events(read_lines(trace_log))

    if not frames:
        problems.append(f"{inspector_log}: no `display-test:frame … cursor=` line; the guest never reported a pointer position")
    if not events:
        problems.append(f"{trace_log}: no `virtio_gpu_update_cursor` event; QEMU was not asked for the trace, or the guest never touched the cursor queue")
    if problems:
        return problems

    first_update = next((index for index, event in enumerate(events) if event[3] == "update" and event[4] != 0), None)
    if first_update is None:
        problems.append(f"{trace_log}: no `update` with a cursor resource; the device never received the cursor image")
        first_update = 0

    moves = {(x, y) for _, x, y, kind, _ in events[first_update:] if kind == "move"}
    missing = [(sequence, x, y) for sequence, x, y in frames if (x, y) not in moves]
    for sequence, x, y in missing:
        problems.append(f"frame {sequence}: the guest moved the cursor to {x},{y} and the device logged no move there")

    if not problems:
        print(
            f"{len(frames)} guest positions, all among the {len(moves)} distinct moves the device logged after the cursor image"
        )
    return problems


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    try:
        problems = check(Path(argv[1]), Path(argv[2]))
    except CheckError as error:
        print(error, file=sys.stderr)
        return 1
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
