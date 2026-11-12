# /// script
# requires-python = ">=3.11"
# ///
"""Select the CI lanes a diff reaches.

Reads `lexoliu/affected-crates`' JSON report from `AFFECTED_REPORT` and the
lane table (`.github/ci-lanes.toml`, which documents the selection rule),
then writes three `GITHUB_OUTPUT` keys for `ci.yml`:

- `lanes`: a JSON object naming every lane, `true` when it runs.
- `matrix`: a JSON list of the selected lanes that run in the `lanes`
  matrix job, one object of settings each.
- `prebuild`: a JSON list of the kernel-prebuild jobs those lanes need, one
  `{target, save-cache}` object per target.

The table is checked against the report's workspace members before
anything is selected: a lane naming a package that does not exist would
otherwise never run, so the script fails naming it.

Usage:

    AFFECTED_REPORT=<report JSON> ci_lanes.py --lanes .github/ci-lanes.toml
"""

import argparse
import json
import os
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

# The keys a `[[lane]]` entry may carry. Settings only a matrix lane reads
# are refused on a lane that has no `command`, so a misplaced key fails here
# instead of being silently ignored.
TRIGGER_KEYS = {"packages", "all-except", "direct"}
MATRIX_KEYS = {"command", "prebuild", "cache", "save-cache", "apt", "tools", "uv"}
LANE_KEYS = {"name"} | TRIGGER_KEYS | MATRIX_KEYS
TOP_KEYS = {"prebuild", "groups", "lane"}
PREBUILD_KEYS = {"cache", "cache-writer"}


class LaneTableError(Exception):
    """The lane table does not describe lanes this workspace can run."""


@dataclass(frozen=True)
class Lane:
    name: str
    # `None` means every package (`packages = "any"`) unless `all_except`
    # is set instead.
    packages: frozenset[str] | None
    all_except: frozenset[str] | None
    direct: frozenset[str]
    matrix: dict[str, object] | None


@dataclass(frozen=True)
class Table:
    lanes: tuple[Lane, ...]
    prebuild_cache: str
    prebuild_writer: str


def _string_list(value, where):
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise LaneTableError(f"{where} must be a list of strings, got {value!r}")
    return value


def _expand(names, groups, members, where):
    expanded = set()
    for name in names:
        if name.startswith("@"):
            group = name[1:]
            if group not in groups:
                raise LaneTableError(f"{where} names group {name}, which [groups] does not define")
            expanded |= groups[group]
        else:
            expanded.add(name)
    unknown = sorted(expanded - members)
    if unknown:
        raise LaneTableError(
            f"{where} names {', '.join(unknown)}, which the workspace does not have "
            "(the affected-crates report lists its members)"
        )
    return frozenset(expanded)


def _matrix_settings(entry, name, prebuild_cache):
    settings = {
        "name": name,
        "command": entry["command"],
        "prebuild": entry.get("prebuild", ""),
        "cache": entry.get("cache", ""),
        "apt": " ".join(_string_list(entry.get("apt", []), f"lane {name} apt")),
        "tools": ",".join(_string_list(entry.get("tools", []), f"lane {name} tools")),
        "uv": entry.get("uv", False),
    }
    save = entry.get("save-cache", True)
    if not isinstance(save, bool) or not isinstance(settings["uv"], bool):
        raise LaneTableError(f"lane {name}: save-cache and uv must be booleans")
    if "save-cache" in entry and not settings["cache"]:
        raise LaneTableError(f"lane {name} sets save-cache without a cache key")
    if settings["cache"] == prebuild_cache:
        raise LaneTableError(f"lane {name} uses the prebuild jobs' cache key {prebuild_cache}")
    settings["save-cache"] = bool(settings["cache"]) and save
    return settings


def load_table(path, members):
    """Parse and check the lane table against the workspace `members`."""
    document = tomllib.loads(Path(path).read_text())
    members = frozenset(members)
    extra = set(document) - TOP_KEYS
    if extra:
        raise LaneTableError(f"unknown top-level keys {sorted(extra)}")

    prebuild = document.get("prebuild", {})
    if set(prebuild) != PREBUILD_KEYS:
        raise LaneTableError(f"[prebuild] must set exactly {sorted(PREBUILD_KEYS)}")

    groups = {}
    for group, names in document.get("groups", {}).items():
        groups[group] = _expand(_string_list(names, f"group {group}"), {}, members, f"group {group}")

    lanes = []
    seen = set()
    for entry in document.get("lane", []):
        name = entry.get("name")
        if not isinstance(name, str) or not name:
            raise LaneTableError(f"a [[lane]] has no name: {entry!r}")
        if name in seen:
            raise LaneTableError(f"lane {name} is defined twice")
        seen.add(name)
        extra = set(entry) - LANE_KEYS
        if extra:
            raise LaneTableError(f"lane {name} has unknown keys {sorted(extra)}")
        if ("packages" in entry) == ("all-except" in entry):
            raise LaneTableError(f"lane {name} must set exactly one of packages and all-except")

        packages = entry.get("packages")
        if packages == "any":
            packages = None
        elif packages is not None:
            packages = _expand(_string_list(packages, f"lane {name} packages"), groups, members, f"lane {name}")
        all_except = entry.get("all-except")
        if all_except is not None:
            all_except = _expand(_string_list(all_except, f"lane {name} all-except"), groups, members, f"lane {name}")
        direct = _expand(_string_list(entry.get("direct", []), f"lane {name} direct"), groups, members, f"lane {name}")

        if "command" in entry:
            matrix = _matrix_settings(entry, name, prebuild["cache"])
        else:
            misplaced = sorted(set(entry) & (MATRIX_KEYS - {"command"}))
            if misplaced:
                raise LaneTableError(f"lane {name} has no command, so {misplaced} would never be read")
            matrix = None
        lanes.append(Lane(name, packages, all_except, direct, matrix))

    return Table(tuple(lanes), prebuild["cache"], prebuild["cache-writer"])


def triggers(lane, report):
    """The packages that select `lane`, or `["workspace"]` when the diff can
    reach every package; empty when the lane does not run."""
    if report["workspace"]:
        return ["workspace"]
    affected = set(report["affected"])
    owners = {owner for owner in report["owners"].values() if owner is not None}
    if lane.all_except is not None:
        hit = affected - lane.all_except
    elif lane.packages is None:
        hit = affected
    else:
        hit = affected & lane.packages
    return sorted(hit | (owners & lane.direct))


def select(table, report):
    """The three outputs of the module docstring, plus each lane's triggers."""
    reasons = {lane.name: triggers(lane, report) for lane in table.lanes}
    lanes = {name: bool(hit) for name, hit in reasons.items()}
    matrix = [lane.matrix for lane in table.lanes if lane.matrix is not None and lanes[lane.name]]
    targets = sorted({entry["prebuild"] for entry in matrix if entry["prebuild"]})
    prebuild = [
        {"target": target, "save-cache": target == table.prebuild_writer} for target in targets
    ]
    return {"lanes": lanes, "matrix": matrix, "prebuild": prebuild}, reasons


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--lanes", required=True, help="the lane table")
    args = parser.parse_args()

    report = json.loads(os.environ["AFFECTED_REPORT"])
    try:
        table = load_table(args.lanes, report["members"])
    except LaneTableError as error:
        print(f"::error file={args.lanes}::{error}", file=sys.stderr)
        sys.exit(1)
    outputs, reasons = select(table, report)

    # The log says why each lane runs or does not: the packages that
    # selected it, `workspace`, or nothing.
    print(json.dumps({"selected-by": reasons, **outputs}, indent=2))
    with open(os.environ["GITHUB_OUTPUT"], "a") as handle:
        for key, value in outputs.items():
            handle.write(f"{key}={json.dumps(value, separators=(',', ':'))}\n")


if __name__ == "__main__":
    main()
