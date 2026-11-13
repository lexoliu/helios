"""The alternating rounds, host gate and stated asymmetries of a lane (#491).

The run records read here have the shape the two drivers write
(`run`, then one `iteration` record per launch); the pins are the real
manifest's, read from where it states them.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import THRESHOLDS, WORKLOADS, iterations, make_report, raw_side

from helios_bench import runner
from helios_bench.assemble import assemble_report, build_cell, build_control
from helios_bench.asymmetries import collect_asymmetries
from helios_bench.host_gate import LINUX_NOTE, HostGateRecorder, gate_deviations
from helios_bench.manifest import Lane, load_manifest, validate_rounds
from helios_bench.render import render_docs_results, render_pins, render_tables
from helios_bench.report import Iteration, Side
from helios_bench.runner import GAP_BENCH, PlannedCommand, RunOptions, execute_gated, plan, read_sides
from helios_bench.sources import SIDE_JSONL
from helios_bench.wasi_apps import gap_bench
from helios_bench.workloads import load_workloads

SIDES = frozenset({Side.HELIOS, Side.LINUX_NATIVE, Side.LINUX_WASMTIME})


def lane_options(tmp_path: Path, **overrides) -> RunOptions:
    return RunOptions(
        lane=load_manifest().lane("aarch64-hvf"),
        out_dir=tmp_path,
        advisory=True,
        **{"sides": SIDES, **overrides},
    )


def pooled(first: list[float], second: list[float]) -> list[Iteration]:
    """Two rounds of one cell: each round is `warmup_discard` cold launches then its warm series."""
    return [
        Iteration(round=number, index=index, elapsed_ms=value, cold=index == 1, metrics={})
        for number, values in enumerate((first, second), 1)
        for index, value in enumerate(values, 1)
    ]


def series(center: float, count: int = 11) -> list[float]:
    """A tight series with a slow first launch, like a real cold iteration."""
    return [center * 2.0] + [center * (1.0 + 0.002 * (index % 3)) for index in range(count - 1)]


# Alternating order.


@pytest.mark.parametrize("rounds", [1, 3, 5, 0, -2])
def test_an_odd_or_too_small_round_count_fails_fast(rounds: int) -> None:
    with pytest.raises(ValueError, match="even number of rounds"):
        validate_rounds(rounds)


def test_a_lane_with_an_odd_round_count_is_refused() -> None:
    raw = load_manifest().lane("aarch64-hvf").model_dump()
    with pytest.raises(ValueError, match="even number of rounds"):
        Lane.model_validate({**raw, "rounds": 3})


def test_the_run_setting_is_validated_too(tmp_path: Path) -> None:
    with pytest.raises(SystemExit, match="--rounds: .*even number of rounds"):
        lane_options(tmp_path, rounds=3)
    assert lane_options(tmp_path).effective_rounds == 2
    assert lane_options(tmp_path, rounds=4).effective_rounds == 4


def test_a_lane_without_rounds_is_one_pass_helios_then_linux(tmp_path: Path) -> None:
    options = RunOptions(
        lane=load_manifest().lane("x86-64-kvm"), out_dir=tmp_path, advisory=True, sides=SIDES
    )
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    timed = [(command.side, command.round) for command in commands if command.side]
    assert timed == [("helios", 1), ("linux", 1)]
    assert str(tmp_path / "helios") in commands[-2].argv or str(tmp_path / "helios") in commands[-1].argv


def test_consecutive_rounds_swap_which_side_goes_first(tmp_path: Path) -> None:
    options = lane_options(tmp_path, rounds=4)
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    timed = [(command.round, command.side) for command in commands if command.side]
    assert timed == [
        (1, "helios"),
        (1, "linux"),
        (2, "linux"),
        (2, "helios"),
        (3, "helios"),
        (3, "linux"),
        (4, "linux"),
        (4, "helios"),
    ]
    for command in commands:
        if command.side:
            assert str(tmp_path / f"round-{command.round}") in " ".join(command.argv)
    # Only the first Helios pass takes the untimed profile boot.
    helios = [command for command in commands if command.side == "helios"]
    assert ["--helios-no-profile-boot" in command.argv for command in helios] == [False, True, True, True]


def test_the_driver_parses_every_round_the_plan_emits(tmp_path: Path) -> None:
    parser = gap_bench().build_parser()
    commands = plan(lane_options(tmp_path), load_manifest(), load_workloads()["workloads"])
    invocations = [command.argv for command in commands if command.argv[1:2] == [str(GAP_BENCH)]]
    assert len(invocations) == 4
    for argv in invocations:
        parser.parse_args(argv[2:])


def test_alternation_is_not_defined_with_a_paired_run_or_a_missing_side(tmp_path: Path) -> None:
    with pytest.raises(SystemExit, match="paired"):
        lane_options(tmp_path, profile_use=tmp_path / "kernel.profdata")
    with pytest.raises(SystemExit, match="both sides in every round"):
        RunOptions(
            lane=load_manifest().lane("aarch64-hvf"),
            out_dir=tmp_path,
            advisory=True,
            sides=frozenset({Side.HELIOS}),
        )


#: What the serial line adds to a Helios launch's host-side round trip in
#: these fixtures; the aarch64-hvf lane's compared value leaves it out.
SERIAL_TRANSPORT_MS = 12.5


def timings(side: Side, value: float) -> dict:
    """The timing fields of one iteration record whose launch took ``value``.

    A Helios program launch carries the inspector's host-side round trip,
    the launch plus the transport, and the kernel's own launch duration;
    a Linux one carries the in-guest time alone.
    """
    if side in runner.HELIOS_SIDES:
        return {"runner": "program", "elapsed_ms": value + SERIAL_TRANSPORT_MS, "launch_elapsed_ms": value}
    return {"elapsed_ms": value}


def write_round(tmp_path: Path, number: int, side: Side, workload: str, values: list[float]) -> None:
    out_dir = tmp_path / f"round-{number}" / runner.SIDE_OUT[side]
    out_dir.mkdir(parents=True, exist_ok=True)
    lines = [json.dumps({"type": "run", "schema_version": 1, "side": side.value})]
    lines += [
        json.dumps(
            {
                "type": "iteration",
                "workload": workload,
                "iteration": index,
                **timings(side, value),
                "metrics": {},
            }
        )
        for index, value in enumerate(values, 1)
    ]
    (out_dir / SIDE_JSONL[side]).write_text("\n".join(lines) + "\n", encoding="utf-8")


def test_the_rounds_of_a_cell_are_pooled_from_their_own_directories(tmp_path: Path) -> None:
    options = lane_options(tmp_path, rounds=2, sides=frozenset({Side.HELIOS, Side.LINUX_WASMTIME}))
    for number in (1, 2):
        write_round(tmp_path, number, Side.HELIOS, "hostcall-loop", series(20.0))
        write_round(tmp_path, number, Side.LINUX_WASMTIME, "hostcall-loop", series(40.0))
    sides = read_sides(options, THRESHOLDS)
    cell = build_cell(Side.HELIOS, sides[Side.HELIOS].cells["hostcall-loop"].iterations, THRESHOLDS, 0.02)
    assert [it.round for it in cell.iterations] == [1] * 11 + [2] * 11
    assert cell.cold.count == 2 and cell.warm.count == 20
    assert len(cell.round_medians) == 2 and not cell.rejected


def test_a_workload_one_round_failed_is_a_failed_cell(tmp_path: Path) -> None:
    options = lane_options(tmp_path, rounds=2, sides=frozenset({Side.HELIOS, Side.LINUX_WASMTIME}))
    write_round(tmp_path, 1, Side.HELIOS, "hostcall-loop", series(20.0))
    write_round(tmp_path, 1, Side.LINUX_WASMTIME, "hostcall-loop", series(40.0))
    write_round(tmp_path, 2, Side.LINUX_WASMTIME, "hostcall-loop", series(40.0))
    out_dir = tmp_path / "round-2" / "helios"
    out_dir.mkdir(parents=True)
    (out_dir / "helios.jsonl").write_text(
        '{"type": "run"}\n'
        '{"type": "failure", "workload": "hostcall-loop", "error": "guest stopped answering"}\n',
        encoding="utf-8",
    )
    sides = read_sides(options, THRESHOLDS)
    assert sides[Side.HELIOS].cells["hostcall-loop"].failure == "round 2: guest stopped answering"
    assert sides[Side.LINUX_WASMTIME].cells["hostcall-loop"].failure is None


def test_a_round_missing_a_workload_is_a_broken_run(tmp_path: Path) -> None:
    options = lane_options(tmp_path, rounds=2, sides=frozenset({Side.HELIOS, Side.LINUX_WASMTIME}))
    for number in (1, 2):
        write_round(tmp_path, number, Side.LINUX_WASMTIME, "hostcall-loop", series(40.0))
    write_round(tmp_path, 1, Side.HELIOS, "hostcall-loop", series(20.0))
    write_round(tmp_path, 2, Side.HELIOS, "quickjs-loop", series(20.0))
    with pytest.raises(SystemExit, match="recorded nothing for hostcall-loop in round"):
        read_sides(options, THRESHOLDS)


def test_rounds_that_agree_within_the_floor_pool_into_one_series() -> None:
    cell = build_cell(Side.HELIOS, pooled(series(20.0), series(20.2)), THRESHOLDS, round_floor=0.03)
    assert not cell.rejected and cell.rejection_reason is None
    assert cell.warm.count == 20 and cell.cold.count == 2
    assert cell.round_medians == pytest.approx([20.0, 20.2], rel=0.01)


def test_rounds_that_disagree_beyond_the_floor_reject_the_cell_like_a_dispersed_one() -> None:
    cell = build_cell(Side.HELIOS, pooled(series(20.0), series(24.0)), THRESHOLDS, round_floor=0.03)
    assert cell.rejected
    assert cell.rejection_reason is not None
    assert "round medians disagree by 0.2" in cell.rejection_reason
    assert "noise floor 0.030" in cell.rejection_reason
    # The reporting of a rejected cell is the one a dispersed cell has: the
    # table marks it, and no comparison is built from it.
    report = disagreeing_report()
    helios = report.workload("hostcall-loop")
    assert helios.cells[Side.HELIOS].rejected and helios.comparisons == []
    assert "(rejected)" in render_tables(report)


def test_a_cell_in_several_rounds_needs_a_floor() -> None:
    with pytest.raises(ValueError, match="no control workload"):
        build_cell(Side.HELIOS, pooled(series(20.0), series(20.0)), THRESHOLDS, round_floor=None)


def test_a_single_round_cell_is_what_it_was_before_rounds() -> None:
    cell = build_cell(Side.HELIOS, iterations(20.0, 0.4, seed=3), THRESHOLDS)
    assert cell.warm.count == 10 and cell.round_medians == []


def disagreeing_report():
    control = build_control(
        "quickjs-loop",
        {
            Side.HELIOS: (
                raw_side({"quickjs-loop": iterations(100.0, 1.0, 1)}),
                raw_side({"quickjs-loop": iterations(100.5, 1.0, 2)}),
            )
        },
        THRESHOLDS,
    )
    template = make_report({"hostcall-loop": 20.0, "quickjs-loop": 100.5, "fs-smallfiles": 25.0})
    sides = {
        Side.HELIOS: raw_side({"hostcall-loop": pooled(series(20.0), series(30.0))}),
        Side.LINUX_WASMTIME: raw_side({"hostcall-loop": iterations(200.0, 4.0, 11)}),
    }
    return assemble_report(
        [WORKLOADS[0]],
        sides,
        control,
        template.run.model_copy(update={"rounds": 2}),
        template.hardware,
        template.pins,
        THRESHOLDS,
    )


# Host gate.


def scripted(levels: list[int]):
    remaining = iter(levels)
    return lambda: next(remaining)


def test_macos_reads_pressure_around_every_side_of_every_round(tmp_path: Path, monkeypatch) -> None:
    gate = HostGateRecorder(system="Darwin", read=scripted([1] * 8))
    ran = []
    monkeypatch.setattr(runner, "execute", lambda command: ran.append((command.side, command.round)))
    commands = plan(lane_options(tmp_path), load_manifest(), load_workloads()["workloads"])
    for command in commands:
        execute_gated(command, gate)
    record = gate.record()
    assert record.applies and not record.inconclusive
    assert [(reading.round, reading.side, reading.moment) for reading in record.readings] == [
        (1, "helios", "before"),
        (1, "helios", "after"),
        (1, "linux", "before"),
        (1, "linux", "after"),
        (2, "linux", "before"),
        (2, "linux", "after"),
        (2, "helios", "before"),
        (2, "helios", "after"),
    ]
    assert gate_deviations(record) == []


def test_a_reading_that_is_not_normal_makes_the_run_inconclusive() -> None:
    gate = HostGateRecorder(system="Darwin", read=scripted([1, 1, 1, 2]))
    gate.take(1, "helios", "before")
    gate.take(1, "helios", "after")
    gate.take(1, "linux", "before")
    gate.take(1, "linux", "after")
    record = gate.record()
    assert record.inconclusive
    assert gate_deviations(record) == [
        "host gate: memory pressure level 2 after the linux side of round 1, not 1 (normal): "
        "the run is inconclusive"
    ]


def test_a_linux_host_is_not_gated_and_the_record_says_so() -> None:
    gate = HostGateRecorder(system="Linux", read=lambda: pytest.fail("a Linux host reads no pressure"))
    gate.take(1, "helios", "before")
    record = gate.record()
    assert not record.applies and record.readings == [] and not record.inconclusive
    assert record.note == LINUX_NOTE and "no equivalent reading" in record.note


def test_an_unknown_host_is_refused() -> None:
    with pytest.raises(SystemExit, match="Darwin and Linux"):
        HostGateRecorder(system="Windows")


def test_a_step_that_times_no_side_is_not_gated() -> None:
    gate = HostGateRecorder(system="Darwin", read=lambda: pytest.fail("no side, no reading"))
    build = PlannedCommand(description="build", argv=["true"], env={}, cwd=Path("."))
    execute_gated(build, gate)


# Stated asymmetries.


def test_the_lane_states_its_asymmetries_and_a_lane_without_them_states_none() -> None:
    manifest = load_manifest()
    assert collect_asymmetries(manifest.lane("x86-64-kvm"), "r", "wasmtime-v1") == []
    stated = collect_asymmetries(manifest.lane("aarch64-hvf"), "1ed7678a", "wasmtime-v48.0.0-aarch64-linux")
    assert [asymmetry.what for asymmetry in stated] == [
        "Wasmtime",
        "Stack-limit interruption",
        "Network class backend",
    ]
    assert stated[2].helios == stated[2].linux == "QEMU `user`"


def test_the_pins_name_the_rounds_the_gate_and_every_asymmetry() -> None:
    manifest = load_manifest()
    lane = manifest.lane("aarch64-hvf")
    report = make_report({"hostcall-loop": 20.0, "quickjs-loop": 100.5, "fs-smallfiles": 25.0})
    gate = HostGateRecorder(system="Darwin", read=scripted([1, 1, 1, 1]))
    for side in ("helios", "linux"):
        gate.take(1, side, "before")
        gate.take(1, side, "after")
    pins = report.pins.model_copy(
        update={
            "asymmetries": collect_asymmetries(
                lane, report.pins.wasmtime_revision, report.pins.wasmtime_linux_release
            )
        }
    )
    report = report.model_copy(
        update={
            "pins": pins,
            "run": report.run.model_copy(update={"rounds": 2, "host_gate": gate.record()}),
        }
    )
    text = render_pins(report)
    assert "| Rounds | 2, alternating" in text
    assert (
        "| Host gate | `kern.memorystatus_vm_pressure_level` read before and after every side of every "
        "round; the run is inconclusive unless it is 1 (normal) throughout: helios before round 1 = 1, "
        "helios after round 1 = 1, linux before round 1 = 1, linux after round 1 = 1; normal throughout |"
    ) in text
    assert "Stack-limit interruption | Helios: compiled into every cwasm; Linux: not compiled in |" in text
    assert (
        "| Differs by construction: Network class backend | Helios: QEMU `user`; Linux: QEMU `user` |" in text
    )
    assert "Differs by construction: Wasmtime | Helios: fork revision `b83d18c8" in text
    assert "Differs by construction" in render_docs_results([report], "1001")
