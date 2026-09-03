"""Readers for the raw JSONL the two harness sides write.

Both the inspector's ``workload-bench`` (Helios) and
``linux_workload_runner.py`` (both Linux sides) write ``iteration`` and
``summary`` records with the same fields, so one reader serves every side.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

from helios_bench.report import Iteration, Side

HELIOS_JSONL = "helios.jsonl"
SIDE_JSONL = {
    Side.HELIOS: "helios.jsonl",
    Side.LINUX_NATIVE: "linux-native.jsonl",
    Side.LINUX_WASMTIME: "linux-wasmtime.jsonl",
}
SIDE_CONTROL_JSONL = {
    Side.HELIOS: "helios-control-{moment}.jsonl",
    Side.LINUX_NATIVE: "linux-native-control-{moment}.jsonl",
    Side.LINUX_WASMTIME: "linux-wasmtime-control-{moment}.jsonl",
}


@dataclass
class RawCell:
    iterations: list[Iteration] = field(default_factory=list)


@dataclass
class RawSide:
    run: dict
    cells: dict[str, RawCell]


def read_side_jsonl(path: Path, warmup_discard: int) -> RawSide:
    run: dict = {}
    cells: dict[str, RawCell] = {}
    with path.open("r", encoding="utf-8") as handle:
        for line in handle:
            if not line.strip():
                continue
            record = json.loads(line)
            if record["type"] == "run":
                run = record
            elif record["type"] == "iteration":
                cell = cells.setdefault(record["workload"], RawCell())
                index = int(record["iteration"])
                cell.iterations.append(
                    Iteration(
                        index=index,
                        elapsed_ms=float(record["elapsed_ms"]),
                        cold=index <= warmup_discard,
                        metrics={name: float(value) for name, value in record.get("metrics", {}).items()},
                    )
                )
    if not run:
        raise SystemExit(f"{path}: no run record")
    return RawSide(run=run, cells=cells)


def read_optional_side(out_dir: Path, side: Side, warmup_discard: int) -> RawSide | None:
    path = out_dir / SIDE_JSONL[side]
    if not path.exists():
        return None
    return read_side_jsonl(path, warmup_discard)


def read_control(out_dir: Path, side: Side, moment: str, warmup_discard: int) -> RawSide | None:
    path = out_dir / SIDE_CONTROL_JSONL[side].format(moment=moment)
    if not path.exists():
        return None
    return read_side_jsonl(path, warmup_discard)
