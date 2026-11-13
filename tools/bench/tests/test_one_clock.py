"""One clock and equal guest RAM for both columns of a lane (#491).

The run records read here have the shape the inspector's `workload-bench`
and `linux_workload_runner.py` write; the lanes are the real manifest's.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import THRESHOLDS

from helios_bench import runner
from helios_bench.assemble import build_cell
from helios_bench.manifest import load_manifest
from helios_bench.render import render_pins, render_tables
from helios_bench.report import Clock, GuestRam, Iteration, Report, Side
from helios_bench.runner import RunOptions, plan
from helios_bench.sources import compared_clock, read_side_jsonl
from helios_bench.wasi_apps import fedora_baseline
from helios_bench.workloads import load_workloads


def write_side(path: Path, records: list[dict]) -> Path:
    lines = [json.dumps({"type": "run", "schema_version": 1})]
    lines += [json.dumps({"type": "iteration", "metrics": {}, **record}) for record in records]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def launch(
    workload: str, index: int, host: float, launch_ms: float | None, runner_name: str = "program"
) -> dict:
    record = {"workload": workload, "runner": runner_name, "iteration": index, "elapsed_ms": host}
    if launch_ms is not None:
        record["launch_elapsed_ms"] = launch_ms
    return record


# Where the clock is chosen.


def test_a_serial_lane_compares_a_helios_launch_on_the_kernels_duration(tmp_path: Path) -> None:
    path = write_side(tmp_path / "helios.jsonl", [launch("process-startup", 1, 31.25, 8.3125)])
    raw = read_side_jsonl(path, Side.HELIOS, "serial", warmup_discard=1)
    iteration = raw.cells["process-startup"].iterations[0]
    assert iteration.clock is Clock.KERNEL_LAUNCH
    assert iteration.elapsed_ms == 8.3125
    # Both values stay in the report.
    assert iteration.host_elapsed_ms == 31.25
    assert iteration.launch_elapsed_ms == 8.3125


def test_a_vsock_lane_keeps_the_host_round_trip(tmp_path: Path) -> None:
    path = write_side(tmp_path / "helios.jsonl", [launch("process-startup", 1, 9.5, 8.25)])
    raw = read_side_jsonl(path, Side.HELIOS_BASELINE, "vsock", warmup_discard=1)
    iteration = raw.cells["process-startup"].iterations[0]
    assert iteration.clock is Clock.HOST_ROUND_TRIP
    assert iteration.elapsed_ms == 9.5
    assert iteration.launch_elapsed_ms == 8.25


def test_a_workload_that_is_no_launch_keeps_the_host_clock_on_a_serial_lane(tmp_path: Path) -> None:
    path = write_side(tmp_path / "helios.jsonl", [launch("aot-curl", 1, 90.5, None, "helios-aot")])
    raw = read_side_jsonl(path, Side.HELIOS, "serial", warmup_discard=1)
    iteration = raw.cells["aot-curl"].iterations[0]
    assert iteration.clock is Clock.HOST_ROUND_TRIP
    assert iteration.elapsed_ms == 90.5
    assert iteration.launch_elapsed_ms is None


def test_a_linux_row_is_timed_inside_its_guest(tmp_path: Path) -> None:
    path = write_side(
        tmp_path / "linux-wasmtime.jsonl",
        [{"workload": "process-startup", "side": "linux-wasmtime", "iteration": 1, "elapsed_ms": 249.0}],
    )
    for transport in ("serial", "vsock"):
        raw = read_side_jsonl(path, Side.LINUX_WASMTIME, transport, warmup_discard=1)
        iteration = raw.cells["process-startup"].iterations[0]
        assert iteration.clock is Clock.GUEST_PROCESS
        assert iteration.elapsed_ms == 249.0
        assert iteration.host_elapsed_ms is None and iteration.launch_elapsed_ms is None


def test_a_serial_lane_refuses_a_launch_without_the_kernels_duration(tmp_path: Path) -> None:
    """An inspector that predates the field is not read on the host clock instead."""
    path = write_side(tmp_path / "helios.jsonl", [launch("process-startup", 1, 31.25, None)])
    with pytest.raises(SystemExit, match="carries no launch_elapsed_ms"):
        read_side_jsonl(path, Side.HELIOS, "serial", warmup_discard=1)


def test_an_unknown_runner_is_refused() -> None:
    with pytest.raises(SystemExit, match="runner 'mystery'"):
        compared_clock(Side.HELIOS, "mystery", "serial")
    with pytest.raises(SystemExit, match="runner None"):
        compared_clock(Side.HELIOS, None, "vsock")


def test_a_cell_states_its_clock_and_refuses_to_pool_two() -> None:
    def series(clock: Clock) -> list[Iteration]:
        return [
            Iteration(index=index, elapsed_ms=10.0 + 0.01 * index, clock=clock, cold=index == 1, metrics={})
            for index in range(1, 12)
        ]

    assert build_cell(Side.HELIOS, series(Clock.KERNEL_LAUNCH), THRESHOLDS).clock is Clock.KERNEL_LAUNCH
    mixed = series(Clock.KERNEL_LAUNCH)[:6] + series(Clock.HOST_ROUND_TRIP)[6:]
    with pytest.raises(ValueError, match="different clocks"):
        build_cell(Side.HELIOS, mixed, THRESHOLDS)


# What the report says about it.


def clocked(report: Report, rpc_transport: str, aot: str) -> Report:
    """``report`` as a lane on ``rpc_transport`` reads it: each cell's
    iterations carry the clock `compared_clock` picks, with ``aot`` the
    one Helios workload that is no program launch."""
    workloads = []
    for workload in report.workloads:
        cells = {}
        for side, cell in workload.cells.items():
            runner_name = "helios-aot" if workload.name == aot else "program"
            clock = compared_clock(side, runner_name, rpc_transport)
            cells[side] = cell.model_copy(update={"clock": clock})
        workloads.append(workload.model_copy(update={"cells": cells}))
    pins = report.pins.model_copy(update={"rpc_transport": rpc_transport})
    return report.model_copy(update={"workloads": workloads, "pins": pins})


def test_the_tables_name_each_compared_rows_clock(baseline_report: Report) -> None:
    report = clocked(baseline_report, "serial", aot="quickjs-loop")
    tables = render_tables(report)
    assert "Helios compared on the kernel's launch duration from the `exec` reply: `hostcall-loop`" in tables
    assert (
        "Helios compared on the host-side round trip of the inspector RPC, being no program launch: "
        "no `exec` reply carries a launch duration: `quickjs-loop`"
    ) in tables
    assert "Linux + Wasmtime compared on the in-guest time around the process: `hostcall-loop`" in tables


def test_a_vsock_lane_names_the_host_clock_without_excuse(baseline_report: Report) -> None:
    report = clocked(baseline_report, "vsock", aot="quickjs-loop")
    tables = render_tables(report)
    assert "Helios compared on the host-side round trip of the inspector RPC: `hostcall-loop`" in tables
    assert "being no program launch" not in tables


# Equal guest RAM.


def test_every_helios_boot_asks_for_preallocated_ram(tmp_path: Path) -> None:
    options = RunOptions(
        lane=load_manifest().lane("aarch64-hvf"),
        out_dir=tmp_path,
        advisory=True,
        sides=frozenset({Side.HELIOS, Side.LINUX_NATIVE, Side.LINUX_WASMTIME}),
    )
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    helios = [command for command in commands if "--skip-linux" in command.argv]
    assert helios, "the plan has a Helios invocation"
    for command in helios:
        assert command.env["HELIOS_WORKLOAD_BENCH_GUEST_RAM"] == "prealloc"


def test_the_fedora_guests_backing_is_read_from_its_script() -> None:
    lane = load_manifest().lane("aarch64-hvf")
    ram = runner.linux_guest_ram(lane)
    spec = fedora_baseline().guest_ram_backend(lane.linux_vm_memory)
    assert ram.backing == "prealloc"
    assert ram.size == lane.linux_vm_memory
    assert f"prealloc-threads={ram.prealloc_threads}" in spec


def test_the_pins_show_both_guests_backed_alike(baseline_report: Report) -> None:
    lane = load_manifest().lane("aarch64-hvf")
    linux = runner.linux_guest_ram(lane)
    # The inspector's run record, as `--guest-ram prealloc` writes it.
    helios_run = {
        "type": "run",
        "vm": {
            "memory": lane.memory,
            "guest_ram": {"backing": "prealloc", "prealloc_threads": linux.prealloc_threads},
        },
    }
    helios = runner.helios_guest_ram(helios_run)
    assert helios == linux
    pins = baseline_report.pins.model_copy(update={"helios_guest_ram": helios, "linux_guest_ram": linux})
    text = render_pins(baseline_report.model_copy(update={"pins": pins}))
    assert (
        f"| Guest RAM | Helios and Linux alike: {lane.memory}, `memory-backend-ram` preallocated by "
        f"{linux.prealloc_threads} threads |"
    ) in text


def test_the_pins_flag_guests_backed_apart(baseline_report: Report) -> None:
    lazy = GuestRam(backing="lazy", size="2G")
    prealloc = GuestRam(backing="prealloc", size="2G", prealloc_threads=8)
    pins = baseline_report.pins.model_copy(update={"helios_guest_ram": lazy, "linux_guest_ram": prealloc})
    text = render_pins(baseline_report.model_copy(update={"pins": pins}))
    assert (
        "Helios: 2G, mapped lazily; Linux: 2G, `memory-backend-ram` preallocated by 8 threads; **not equal**"
    ) in text


def test_a_helios_run_record_without_its_ram_backing_is_refused() -> None:
    with pytest.raises(SystemExit, match="predates --guest-ram"):
        runner.helios_guest_ram({"type": "run", "vm": {"memory": "2G"}})
