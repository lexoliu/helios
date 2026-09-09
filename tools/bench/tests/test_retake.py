"""The retake: a headline cell too dispersed to gate on is measured again
inside the job, on every Helios image, before the gate reads the report.

Run 34390597958 blocked PR #280 on identical kernels because one baseline
cell of `tcp-latency` came back at warm CV 0.15012 against the 0.150
bound (#295). The hour was spent; the fix is a minute of re-measurement.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import THRESHOLDS, iterations, raw_side

from helios_bench import runner
from helios_bench.baseline import Baseline
from helios_bench.gate import evaluate_paired, gate_report
from helios_bench.manifest import load_manifest
from helios_bench.render import render_gate
from helios_bench.report import Report, Side
from helios_bench.runner import (
    RETAKE_OUT,
    RunOptions,
    dispersed_headline_workloads,
    retake,
    retake_plan,
)
from helios_bench.wasi_apps import gap_bench

HEADLINE = {"name": "hostcall-loop", "class": "hostcall", "headline": True}
QUIET = {"name": "quickjs-loop", "class": "compute", "headline": True}
SIDE_SHOW = {"name": "fs-smallfiles", "class": "fs", "headline": False}
WORKLOADS = [HEADLINE, QUIET, SIDE_SHOW]


def tight(center: float, seed: int):
    return iterations(center, center * 0.02, seed)


def dispersed(center: float, seed: int):
    return iterations(center, center * 0.4, seed)


@pytest.fixture
def options(tmp_path) -> RunOptions:
    return RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path / "out",
        advisory=True,
        sides=frozenset({Side.HELIOS, Side.HELIOS_BASELINE}),
        baseline=Baseline(ref="merge-base", sha="a" * 40, worktree=tmp_path / "worktree"),
    )


def sides_with(baseline_headline, helios_sideshow):
    return {
        Side.HELIOS: raw_side(
            {
                "hostcall-loop": tight(20.0, 1),
                "quickjs-loop": tight(100.0, 2),
                "fs-smallfiles": helios_sideshow,
            }
        ),
        Side.HELIOS_BASELINE: raw_side(
            {
                "hostcall-loop": baseline_headline,
                "quickjs-loop": tight(100.0, 4),
                "fs-smallfiles": tight(25.0, 5),
            }
        ),
    }


def write_retake(out_dir: Path, side: Side, name: str, values) -> None:
    path = out_dir / RETAKE_OUT / runner.SIDE_OUT[side] / "helios.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [json.dumps({"type": "run", "schema_version": 1})]
    for iteration in values:
        lines.append(
            json.dumps(
                {
                    "type": "iteration",
                    "workload": name,
                    "iteration": iteration.index,
                    "elapsed_ms": iteration.elapsed_ms,
                    "metrics": {},
                }
            )
        )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def test_a_clean_run_plans_no_retake(options, monkeypatch) -> None:
    sides = sides_with(baseline_headline=tight(20.0, 3), helios_sideshow=tight(25.0, 6))
    monkeypatch.setattr(
        runner, "execute", lambda command: pytest.fail(f"nothing to retake, yet ran {command}")
    )

    assert dispersed_headline_workloads(sides, WORKLOADS, THRESHOLDS) == []
    assert retake(options, 11, WORKLOADS, sides, THRESHOLDS) == []


def test_only_a_dispersed_headline_cell_is_retaken(options, monkeypatch) -> None:
    """The baseline's headline cell is dispersed and so is the candidate's
    side show; only the headline workload is timed again, on both images,
    and its new cells replace the first pass on both."""
    sides = sides_with(baseline_headline=dispersed(20.0, 3), helios_sideshow=dispersed(25.0, 6))
    first_helios = sides[Side.HELIOS].cells["hostcall-loop"]
    executed = []

    def fake_execute(command):
        executed.append(command)
        write_retake(options.out_dir, Side.HELIOS, "hostcall-loop", tight(21.0, 7))
        write_retake(options.out_dir, Side.HELIOS_BASELINE, "hostcall-loop", tight(20.5, 8))

    monkeypatch.setattr(runner, "execute", fake_execute)

    assert dispersed_headline_workloads(sides, WORKLOADS, THRESHOLDS) == [HEADLINE]
    assert retake(options, 11, WORKLOADS, sides, THRESHOLDS) == ["hostcall-loop"]
    assert len(executed) == 1

    args = gap_bench().build_parser().parse_args(executed[0].argv[2:])
    assert args.workloads == ["hostcall-loop"]
    assert not args.control, "the control pass belongs to the suite, not the retake"
    assert Path(args.out_dir) == options.out_dir / RETAKE_OUT / "helios"
    assert args.helios_baseline_out_dir == options.out_dir / RETAKE_OUT / "helios-baseline"
    assert args.iterations == 11

    for side in (Side.HELIOS, Side.HELIOS_BASELINE):
        cell = sides[side].cells["hostcall-loop"]
        assert cell is not first_helios
        assert runner.build_cell(side, cell.iterations, THRESHOLDS).rejected is False
    # The side show keeps its dispersed first pass: it does not gate.
    assert runner.build_cell(
        Side.HELIOS, sides[Side.HELIOS].cells["fs-smallfiles"].iterations, THRESHOLDS
    ).rejected


def test_a_retake_that_writes_nothing_is_a_failed_run(options, monkeypatch) -> None:
    sides = sides_with(baseline_headline=dispersed(20.0, 3), helios_sideshow=tight(25.0, 6))
    monkeypatch.setattr(runner, "execute", lambda command: None)

    with pytest.raises(SystemExit, match="produced no JSONL"):
        retake(options, 11, WORKLOADS, sides, THRESHOLDS)


def test_an_unpaired_run_retakes_its_one_image(tmp_path, monkeypatch) -> None:
    options = RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path / "out",
        advisory=True,
        sides=frozenset({Side.HELIOS}),
    )
    sides = {Side.HELIOS: raw_side({"hostcall-loop": dispersed(20.0, 3), "quickjs-loop": tight(100.0, 2)})}
    monkeypatch.setattr(
        runner,
        "execute",
        lambda command: write_retake(options.out_dir, Side.HELIOS, "hostcall-loop", tight(21.0, 7)),
    )

    assert retake(options, 11, WORKLOADS, sides, THRESHOLDS) == ["hostcall-loop"]
    args = gap_bench().build_parser().parse_args(retake_plan(options, 11, [HEADLINE]).argv[2:])
    assert args.helios_baseline_out_dir is None


def test_the_gate_names_the_retaken_workloads(paired_flat_report: Report) -> None:
    run = paired_flat_report.run.model_copy(update={"retaken": ["hostcall-loop"]})
    report = paired_flat_report.model_copy(update={"run": run})

    assert evaluate_paired(report).retaken == ["hostcall-loop"]
    text = render_gate(gate_report(report, None), "x86-64-kvm")
    assert (
        "Timed again on every Helios image after a first pass too dispersed to gate on: `hostcall-loop`"
        in text
    )
    assert "Timed again" not in render_gate(
        gate_report(paired_flat_report, None),
        "x86-64-kvm",
    )
