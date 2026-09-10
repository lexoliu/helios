#!/usr/bin/env python3
"""Generates `hal/src/input/codes.rs` from Linux's `input-event-codes.h`.

evdev is the event vocabulary virtio-input carries: a `virtio_input_event`
is a Linux `input_event` minus its timestamp, and the numbers in it are
the ones this header defines. Helios therefore does not invent an event
model, and does not retype the tables by hand either — they are pulled
from a named kernel revision and turned into Rust constants together with
the reverse lookups a diagnostic line needs.

The header is `GPL-2.0-only WITH Linux-syscall-note`, and the note is what
makes this legitimate: it exempts the UAPI constants from the GPL's reach
so a non-GPL program may use the ABI they describe. The
`input-event-codes` crate on crates.io is plain `GPL-2.0-only` with no
such note and its newest release describes Linux 6.2, so it is not the
route here.

Usage:

    tools/gen-input-event-codes.py --revision v7.0

Fetches that revision's header from the kernel's git mirror and rewrites
`hal/src/input/codes.rs` in place, then runs rustfmt over it. Pass
`--header <path>` to generate from a local copy instead.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import urllib.request
from pathlib import Path

HEADER_URL = (
    "https://raw.githubusercontent.com/torvalds/linux/{revision}"
    "/include/uapi/linux/input-event-codes.h"
)

DEFINE = re.compile(r"^#define\s+([A-Za-z_][A-Za-z0-9_]*)\s+(\S.*?)\s*(?:/\*.*)?$")
PLUS_ONE = re.compile(r"^\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\+\s*1\s*\)$")

# One namespace per reverse-lookup function: the `#define` prefixes that
# feed it, the function name, and what a doc comment should call it.
NAMESPACES = [
    (("INPUT_PROP_",), "property_name", "device property (`INPUT_PROP_*`)"),
    (("EV_",), "event_type_name", "event type (`EV_*`)"),
    (("SYN_",), "synchronization_name", "synchronization code (`SYN_*`)"),
    (("KEY_", "BTN_"), "key_name", "key or button code (`KEY_*`, `BTN_*`)"),
    (("REL_",), "relative_axis_name", "relative axis (`REL_*`)"),
    (("ABS_",), "absolute_axis_name", "absolute axis (`ABS_*`)"),
    (("SW_",), "switch_name", "switch code (`SW_*`)"),
    (("MSC_",), "misc_name", "miscellaneous code (`MSC_*`)"),
    (("LED_",), "led_name", "indicator (`LED_*`)"),
    (("REP_",), "repeat_name", "autorepeat parameter (`REP_*`)"),
    (("SND_",), "sound_name", "simple sound (`SND_*`)"),
]

# Bounds and counts, not codes: they name the end of a namespace rather
# than a value a device can report, so they stay constants and never
# reach a reverse lookup.
BOUND_SUFFIXES = ("_MAX", "_CNT")
# `KEY_MIN_INTERESTING` is an alias for `KEY_MUTE` that marks where the
# interesting half of the keyboard namespace starts; it names no code of
# its own. The `BTN_*` entries below are the same shape: each marks
# where a class of buttons begins and shares its value with that class's
# first real button, so excluding them leaves the reverse lookup naming
# `BTN_LEFT` rather than `BTN_MOUSE` — which is what evdev's own tooling
# prints, and what a person reading a trace expects.
BOUND_NAMES = {
    "KEY_MIN_INTERESTING",
    "BTN_MISC",
    "BTN_MOUSE",
    "BTN_JOYSTICK",
    "BTN_GAMEPAD",
    "BTN_DIGI",
    "BTN_WHEEL",
    "BTN_TRIGGER_HAPPY",
}


def parse_header(text: str) -> list[tuple[str, int]]:
    """Every `#define` in the header, in order, with its resolved value."""
    values: dict[str, int] = {}
    ordered: list[tuple[str, int]] = []
    for line in text.splitlines():
        match = DEFINE.match(line)
        if match is None:
            continue
        name, raw = match.group(1), match.group(2).strip()
        if name.startswith("_UAPI"):
            continue
        value = resolve(raw, values)
        if value is None:
            raise SystemExit(f"cannot resolve `#define {name} {raw}`")
        values[name] = value
        ordered.append((name, value))
    return ordered


def resolve(raw: str, values: dict[str, int]) -> int | None:
    """The integer a `#define` body spells, or `None` for a shape this
    generator does not know how to read.

    Three shapes appear in the header: a literal, a reference to an
    earlier define (`BTN_A` is `BTN_SOUTH`), and `(SOMETHING + 1)` for a
    namespace's element count.
    """
    try:
        return int(raw, 0)
    except ValueError:
        pass
    if raw in values:
        return values[raw]
    plus_one = PLUS_ONE.match(raw)
    if plus_one is not None and plus_one.group(1) in values:
        return values[plus_one.group(1)] + 1
    return None


def is_bound(name: str) -> bool:
    return name in BOUND_NAMES or name.endswith(BOUND_SUFFIXES)


def render(revision: str, defines: list[tuple[str, int]]) -> str:
    out: list[str] = []
    out.append(
        f"""//! Linux evdev codes, generated from `include/uapi/linux/input-event-codes.h`.
//!
//! Source revision: Linux `{revision}`
//! (`GPL-2.0-only WITH Linux-syscall-note`; the note exempts the UAPI
//! constants from the licence's reach, which is what lets this tree
//! carry them).
//!
//! Generated by `tools/gen-input-event-codes.py`. Do not edit by hand:
//! re-run the generator against a newer revision instead.
//!
//! A virtio-input device reports exactly these numbers, so they are
//! reproduced rather than translated into a model of this kernel's own
//! (AGENTS.md §3.1: the vocabulary belongs to the device, not to us).
//! Beside the constants sit the reverse lookups a boot line and a trace
//! record need — a code with no name is one this revision does not
//! define, which is a fact about the device, not an error.
"""
    )

    for name, value in defines:
        out.append(f"pub const {name}: u16 = {value:#x};")
    out.append("")

    claimed: set[str] = set()
    for prefixes, function, description in NAMESPACES:
        seen: dict[int, str] = {}
        for name, value in defines:
            if not name.startswith(prefixes) or is_bound(name):
                continue
            # A later `#define` that spells an earlier one's value is an
            # alias (`BTN_A` for `BTN_SOUTH`, `KEY_HANGUEL` for
            # `KEY_HANGEUL`). The first spelling wins, so a name is
            # stable across revisions that add another alias.
            seen.setdefault(value, name)
            claimed.add(name)
        out.append(f"/// The name this revision gives a {description}.")
        out.append(f"pub const fn {function}(code: u16) -> Option<&'static str> {{")
        out.append("    match code {")
        for value in sorted(seen):
            out.append(f'        {value:#x} => Some("{seen[value]}"),')
        out.append("        _ => None,")
        out.append("    }")
        out.append("}")
        out.append("")

    unclaimed = [
        name for name, _ in defines if not is_bound(name) and name not in claimed
    ]
    if unclaimed:
        raise SystemExit(
            "no namespace claims these defines, so they would have no reverse "
            f"lookup: {', '.join(unclaimed)}"
        )
    return "\n".join(out) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--revision",
        default="v7.0",
        help="kernel tag to generate from (default: %(default)s)",
    )
    parser.add_argument(
        "--header",
        type=Path,
        help="read a local copy of the header instead of fetching the tag",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "hal/src/input/codes.rs",
        help="file to write (default: %(default)s)",
    )
    args = parser.parse_args()

    if args.header is not None:
        text = args.header.read_text(encoding="utf-8")
    else:
        url = HEADER_URL.format(revision=args.revision)
        with urllib.request.urlopen(url) as response:
            text = response.read().decode("utf-8")

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(render(args.revision, parse_header(text)), encoding="utf-8")
    subprocess.run(
        ["rustfmt", "--edition", "2024", str(args.output)],
        check=True,
    )
    print(f"wrote {args.output} from Linux {args.revision}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
