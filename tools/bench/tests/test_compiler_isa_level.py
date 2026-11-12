"""The ISA level the Linux column's compile asks for is the one the Helios guest reported."""

from __future__ import annotations

import json
from dataclasses import replace
from pathlib import Path

import pytest

from helios_bench.manifest import load_manifest
from helios_bench.report import Side
from helios_bench.runner import HELIOS_OUT, RunOptions, guest_isa_level_name, plan, run_suite
from helios_bench.wasi_apps import gap_bench
from helios_bench.workloads import load_workloads

BOTH = frozenset({Side.HELIOS, Side.LINUX_NATIVE, Side.LINUX_WASMTIME})
LINUX_ONLY = frozenset({Side.LINUX_NATIVE, Side.LINUX_WASMTIME})


def options_for(tmp_path: Path, sides: frozenset[Side], rounds: int | None, **overrides) -> RunOptions:
    return RunOptions(
        lane=load_manifest().lane("x86-64-kvm").model_copy(update={"rounds": rounds}),
        out_dir=tmp_path,
        advisory=True,
        sides=sides,
        **overrides,
    )


def write_helios_record(out_dir: Path, level: str, extras: list[str]) -> None:
    out_dir.mkdir(parents=True)
    record = {"type": "run", "guest_isa": {"level": level, "extras": extras}}
    (out_dir / "helios.jsonl").write_text(json.dumps(record) + "\n", encoding="utf-8")


def test_the_level_is_the_name_the_kernel_sends(tmp_path) -> None:
    write_helios_record(tmp_path / "with-extras", "x86-64-v4", ["vbmi", "bitalg"])
    write_helios_record(tmp_path / "plain", "x86-64-v3", [])
    assert guest_isa_level_name(tmp_path / "with-extras") == "x86-64-v4+vbmi+bitalg"
    assert guest_isa_level_name(tmp_path / "plain") == "x86-64-v3"


def test_a_helios_run_with_no_record_yet_is_refused_naming_the_option(tmp_path) -> None:
    with pytest.raises(SystemExit, match="--compiler-isa-level"):
        guest_isa_level_name(tmp_path / HELIOS_OUT)


def test_a_helios_record_without_the_level_is_a_broken_run(tmp_path) -> None:
    out = tmp_path / HELIOS_OUT
    out.mkdir()
    (out / "helios.jsonl").write_text(json.dumps({"type": "run"}) + "\n", encoding="utf-8")
    with pytest.raises(SystemExit, match="no `guest_isa`"):
        guest_isa_level_name(out)


@pytest.mark.parametrize("rounds", [None, 2, 4])
def test_every_linux_invocation_reads_the_level_from_the_first_rounds_helios_record(tmp_path, rounds) -> None:
    options = options_for(tmp_path, BOTH, rounds)
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    first_helios = next(index for index, command in enumerate(commands) if command.side == "helios")
    linux = [(index, command) for index, command in enumerate(commands) if command.side == "linux"]
    assert linux
    source = (tmp_path / "round-1" if rounds else tmp_path) / HELIOS_OUT
    assert all(command.isa_level_source == source for _, command in linux)
    # Round 1 runs Helios first, so the record the source names exists before
    # any Linux invocation starts, an even round's included.
    assert first_helios < linux[0][0]
    # The level is not part of the plan; it is read when the invocation starts.
    assert all("--compiler-isa-level" not in command.argv for _, command in linux)


def test_the_resolved_linux_argv_carries_the_level_and_the_driver_parses_it(tmp_path) -> None:
    options = options_for(tmp_path, BOTH, None)
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    (linux,) = [command for command in commands if command.side == "linux"]
    write_helios_record(tmp_path / HELIOS_OUT, "x86-64-v4", ["vbmi", "bitalg"])
    parsed = gap_bench().build_parser().parse_args(linux.resolved()[2:])
    assert parsed.compiler_isa_level == "x86-64-v4+vbmi+bitalg"


def test_a_linux_invocation_that_runs_before_any_helios_record_is_refused(tmp_path) -> None:
    options = options_for(tmp_path, BOTH, None)
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    (linux,) = [command for command in commands if command.side == "linux"]
    with pytest.raises(SystemExit, match="--compiler-isa-level"):
        linux.resolved()


def test_a_run_without_the_helios_side_takes_the_level_from_the_option(tmp_path) -> None:
    options = options_for(tmp_path, LINUX_ONLY, None, compiler_isa_level="x86-64-v3")
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    (linux,) = [command for command in commands if command.side == "linux"]
    assert linux.isa_level_source is None
    parsed = gap_bench().build_parser().parse_args(linux.resolved()[2:])
    assert parsed.compiler_isa_level == "x86-64-v3"


def test_a_run_without_the_helios_side_or_the_option_leaves_the_driver_to_refuse(tmp_path) -> None:
    options = options_for(tmp_path, LINUX_ONLY, None)
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    (linux,) = [command for command in commands if command.side == "linux"]
    assert linux.isa_level_source is None
    assert gap_bench().build_parser().parse_args(linux.resolved()[2:]).compiler_isa_level is None


def test_the_option_beside_the_helios_side_is_refused(tmp_path) -> None:
    options = replace(options_for(tmp_path, BOTH, None), compiler_isa_level="x86-64-v3")
    with pytest.raises(SystemExit, match="--compiler-isa-level"):
        run_suite(options, load_manifest(), dry_run=True)
