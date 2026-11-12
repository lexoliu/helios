"""Readers for the raw JSONL the two harness sides write.

Both the inspector's ``workload-bench`` (Helios) and
``linux_workload_runner.py`` (both Linux sides) write ``iteration``,
``summary`` and ``failure`` records with the same fields, so one reader
serves every side. A ``failure`` record names a workload the side could
not measure and why; the report carries it as a failed cell rather than
a missing one.

The reader is also where each row's compared value is chosen
(`compared_clock`): a Helios iteration carries two clocks, the
inspector's host-side round trip and, for a program launch, the
kernel's own launch duration, and which one a row is compared on
depends on the transport the lane's RPC rode (#491).
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

from helios_bench.report import HELIOS_SIDES, Clock, Iteration, Side

#: The inspector's workload runners whose iteration is one `exec` launch,
#: and so carries the kernel's `launch_elapsed_ms`.
LAUNCH_RUNNERS = frozenset({"program", "shell"})
#: The runners that are no program launch: `helios-aot` is the
#: `programs.aot` RPC, which answers no `exec` reply.
UNLAUNCHED_RUNNERS = frozenset({"helios-aot"})


def compared_clock(side: Side, runner: str | None, rpc_transport: Literal["serial", "vsock"]) -> Clock:
    """The clock a row's compared value is read from: the one place it is chosen (#491).

    A Linux row is timed inside its guest. A Helios row is timed around
    the inspector RPC on the host, which on a `serial` lane puts the
    serial line inside every value; there the compared value is the
    kernel's own launch duration instead, so both columns are timed from
    inside their guests. A Helios workload that is no program launch has
    no such duration and keeps the host clock.
    """
    if side not in HELIOS_SIDES:
        return Clock.GUEST_PROCESS
    if runner in LAUNCH_RUNNERS:
        return Clock.KERNEL_LAUNCH if rpc_transport == "serial" else Clock.HOST_ROUND_TRIP
    if runner in UNLAUNCHED_RUNNERS:
        return Clock.HOST_ROUND_TRIP
    raise SystemExit(
        f"a {side} iteration names runner {runner!r}, which is neither a program launch "
        f"({sorted(LAUNCH_RUNNERS)}) nor a known non-launch ({sorted(UNLAUNCHED_RUNNERS)})"
    )


def read_iteration(
    path: Path,
    record: dict,
    side: Side,
    rpc_transport: Literal["serial", "vsock"],
    warmup_discard: int,
    round_number: int,
) -> Iteration:
    """One iteration record, its compared value read from the clock `compared_clock` picks."""
    index = int(record["iteration"])
    metrics = {name: float(value) for name, value in record.get("metrics", {}).items()}
    clock = compared_clock(side, record.get("runner"), rpc_transport)
    if side not in HELIOS_SIDES:
        return Iteration(
            round=round_number,
            index=index,
            elapsed_ms=float(record["elapsed_ms"]),
            clock=clock,
            cold=index <= warmup_discard,
            metrics=metrics,
        )
    host_elapsed_ms = float(record["elapsed_ms"])
    launch = record.get("launch_elapsed_ms")
    launch_elapsed_ms = None if launch is None else float(launch)
    compared = host_elapsed_ms
    if clock is Clock.KERNEL_LAUNCH:
        if launch_elapsed_ms is None:
            raise SystemExit(
                f"{path}: {record['workload']} iteration {index} is compared on the kernel's "
                f"launch duration on a {rpc_transport} lane and carries no launch_elapsed_ms, "
                "so the inspector that wrote it predates the duration in the exec reply (#491)"
            )
        compared = launch_elapsed_ms
    return Iteration(
        round=round_number,
        index=index,
        elapsed_ms=compared,
        clock=clock,
        host_elapsed_ms=host_elapsed_ms,
        launch_elapsed_ms=launch_elapsed_ms,
        cold=index <= warmup_discard,
        metrics=metrics,
    )


HELIOS_JSONL = "helios.jsonl"
# Every Helios image writes the same file names; what tells them apart is
# the directory the driver was told to write each into.
SIDE_JSONL = {
    Side.HELIOS: "helios.jsonl",
    Side.HELIOS_BASELINE: "helios.jsonl",
    Side.HELIOS_BASELINE_REBUILD: "helios.jsonl",
    Side.LINUX_NATIVE: "linux-native.jsonl",
    Side.LINUX_WASMTIME: "linux-wasmtime.jsonl",
}
SIDE_CONTROL_JSONL = {
    Side.HELIOS: "helios-control-{moment}.jsonl",
    Side.HELIOS_BASELINE: "helios-control-{moment}.jsonl",
    Side.HELIOS_BASELINE_REBUILD: "helios-control-{moment}.jsonl",
    Side.LINUX_NATIVE: "linux-native-control-{moment}.jsonl",
    Side.LINUX_WASMTIME: "linux-wasmtime-control-{moment}.jsonl",
}


@dataclass
class RawCell:
    iterations: list[Iteration] = field(default_factory=list)
    failure: str | None = None


@dataclass
class RawSide:
    run: dict
    cells: dict[str, RawCell]


def read_side_jsonl(
    path: Path,
    side: Side,
    rpc_transport: Literal["serial", "vsock"],
    warmup_discard: int,
    round_number: int = 1,
) -> RawSide:
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
                cell.iterations.append(
                    read_iteration(path, record, side, rpc_transport, warmup_discard, round_number)
                )
            elif record["type"] == "failure":
                cell = cells.setdefault(record["workload"], RawCell())
                cell.failure = str(record["error"])
    if not run:
        raise SystemExit(f"{path}: no run record")
    return RawSide(run=run, cells=cells)


def read_optional_side(
    out_dir: Path,
    side: Side,
    rpc_transport: Literal["serial", "vsock"],
    warmup_discard: int,
    round_number: int = 1,
) -> RawSide | None:
    path = out_dir / SIDE_JSONL[side]
    if not path.exists():
        return None
    return read_side_jsonl(path, side, rpc_transport, warmup_discard, round_number)


def read_control(
    out_dir: Path,
    side: Side,
    rpc_transport: Literal["serial", "vsock"],
    moment: str,
    warmup_discard: int,
) -> RawSide | None:
    path = out_dir / SIDE_CONTROL_JSONL[side].format(moment=moment)
    if not path.exists():
        return None
    return read_side_jsonl(path, side, rpc_transport, warmup_discard)
