#!/usr/bin/env python3
"""Reports what a raw debug-serial capture holds, and whether it is whole.

`helios-inspector vm` binds the guest's debug serial to a QEMU chardev
with `logfile=`, so this file records every byte the guest's 16550
handed the host, in order, before anything on the host frames it. That
is what tells two byte losses apart:

  * a marker already broken here was dropped inside QEMU's transmit
    path — the 16550 model re-arms a writability watch a bounded number
    of times when the host socket is full and then discards the byte;
  * a marker whole here but broken in the inspector's output was lost
    by the reader that frames the line.

The kernel writes a stage marker as `\\n[KDBG <stage>]\\n`, under the
console gate, so in a whole capture every marker occupies a line of its
own. A marker that shares its line with anything else, or that never
closes before the newline, is bytes the guest wrote and the host never
saw. Those are reported and make this exit non-zero; the stage names
themselves are printed rather than checked, because the vocabulary is
open (`program:*`, `*-progress`, `error …`).
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

MARKER_OPEN = b"[KDBG "
CAPTURE_NAME = "debug-serial.log"


def line_bounds(data: bytes, offset: int) -> tuple[int, int]:
    """The line `offset` falls on, as [start, end) without the newline."""
    start = data.rfind(b"\n", 0, offset) + 1
    end = data.find(b"\n", offset)
    return start, len(data) if end == -1 else end


def scan(data: bytes) -> tuple[list[str], list[str]]:
    """The whole markers a capture holds, and the broken ones."""
    whole: list[str] = []
    broken: list[str] = []
    offset = data.find(MARKER_OPEN)
    while offset != -1:
        start, end = line_bounds(data, offset)
        line = data[start:end]
        text = line.decode("utf-8", "replace")
        if start == offset and line.endswith(b"]") and line.count(MARKER_OPEN) == 1:
            whole.append(text[len(MARKER_OPEN) : -1])
        else:
            broken.append(text)
        offset = data.find(MARKER_OPEN, end if end > offset else offset + 1)
    return whole, broken


def report(capture: Path) -> bool:
    data = capture.read_bytes()
    whole, broken = scan(data)
    print(f"\n=== {capture} ({len(data)} bytes) ===")
    print(f"markers: {', '.join(whole) if whole else '(none)'}")
    for line in broken:
        print(f"broken marker line: {line!r}")
    return not broken


def captures_under(argument: str) -> list[Path]:
    """The captures a command-line argument names.

    A runtime directory holds one per VM the lane booted, so a lane
    passes the directory and gets every session's capture checked.
    """
    path = Path(argument)
    if path.is_dir():
        return sorted(path.glob(f"**/{CAPTURE_NAME}"))
    return [path]


def main(arguments: list[str]) -> int:
    if not arguments:
        print(__doc__)
        return 2
    captures = [
        capture for argument in arguments for capture in captures_under(argument)
    ]
    missing = [capture for capture in captures if not capture.is_file()]
    for capture in missing:
        print(f"no debug serial capture at {capture}")
    present = [capture for capture in captures if capture.is_file()]
    if not present:
        print("no debug serial capture was produced")
        return 1
    whole = all([report(capture) for capture in present])
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as step_summary:
            step_summary.write("### Raw debug serial capture\n\n")
            for capture in present:
                markers, broken = scan(capture.read_bytes())
                step_summary.write(f"`{capture}`\n\n```\n")
                step_summary.write(f"markers: {', '.join(markers)}\n")
                for line in broken:
                    step_summary.write(f"broken marker line: {line!r}\n")
                step_summary.write("```\n\n")
    if not whole:
        print(
            "\nA stage marker reached the host already broken: the bytes were "
            "lost below the inspector, in QEMU's serial transmit path."
        )
        return 1
    print("\nEvery stage marker in the raw capture is whole.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
