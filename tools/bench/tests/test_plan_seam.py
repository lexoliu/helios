"""The seam between `helios-bench` and the driver it invokes.

`helios_bench.runner` writes the argv and `linux-gap-bench.py` reads it,
from two files that are edited together and otherwise only meet forty
minutes into a lane. Every flag the plan emits is parsed here by the very
parser that will receive it, and the workload selection it implies is run
to the same conclusion.
"""

from __future__ import annotations

import pytest

from helios_bench.manifest import load_manifest
from helios_bench.report import Side
from helios_bench.runner import GAP_BENCH, RunOptions, plan
from helios_bench.wasi_apps import gap_bench
from helios_bench.workloads import load_workloads

SKIPPED = ("instance-startup-100", "instance-startup-500")


def driver_invocations(options: RunOptions, workloads: list[dict]) -> list[list[str]]:
    commands = plan(options, load_manifest(), workloads)
    return [command.argv for command in commands if command.argv[1:2] == [str(GAP_BENCH)]]


@pytest.fixture
def options(tmp_path) -> RunOptions:
    return RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path,
        advisory=True,
        sides=frozenset({Side.HELIOS, Side.LINUX_NATIVE, Side.LINUX_WASMTIME}),
        skip_linux_workloads=SKIPPED,
    )


def test_the_driver_parses_every_flag_the_plan_emits(options) -> None:
    workloads = load_workloads()["workloads"]
    invocations = driver_invocations(options, workloads)
    assert len(invocations) == 2, "one invocation per side of the comparison"
    parser = gap_bench().build_parser()
    for argv in invocations:
        parser.parse_args(argv[2:])


def test_the_linux_side_subtracts_what_it_cannot_compare(options) -> None:
    """A name asked for and skipped in the same breath is skipped.

    The plan names every workload on both sides and then subtracts, on the
    Linux side, the ones whose Helios half cannot be measured (#130). The
    driver used to refuse those names as `unknown or filtered`, which
    ended the run on its first Linux command.
    """
    driver = gap_bench()
    manifest = load_workloads()
    workloads = manifest["workloads"]
    parser = driver.build_parser()
    selections = {}
    for argv in driver_invocations(options, workloads):
        args = parser.parse_args(argv[2:])
        side = "helios" if args.skip_linux else "linux"
        selected = driver.selected_workloads(manifest, args.classes, args.workloads, args.skip_workloads)
        selections[side] = {workload["name"] for workload in selected}

    assert set(SKIPPED) <= selections["helios"], (
        "the Helios side still measures them, and records the failure"
    )
    assert not set(SKIPPED) & selections["linux"]
    assert selections["helios"] - set(SKIPPED) == selections["linux"]


def test_an_unknown_skip_is_still_refused() -> None:
    driver = gap_bench()
    with pytest.raises(SystemExit):
        driver.selected_workloads(load_workloads(), [], [], ["no-such-workload"])
