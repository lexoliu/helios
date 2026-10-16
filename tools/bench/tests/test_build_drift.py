"""Each paired row against the drift the job measured for it (#479).

Run 38940834451 paired dev `8dacde88` against an empty commit on top of
it — identical code — with each column's kernel built against its own
kernel-profile collection, and the gate drew significant verdicts on it:
`fs-smallfiles` -11.2%, `sched-tasks` -2.4%, `instance-startup-100`
-1.4%, `instance-startup-500` -2.6%. Read the other way round, the same
numbers are regressions that block. Two sources of movement the gate did
not measure are behind them: how far one image moves between two of its
boots, and how far two builds of one commit move apart.

The A/A fixture is derived from that run's report and raw JSONL, and
holds only the workloads that took a verdict and the metrics behind it.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import THRESHOLDS, iterations
from conftest import paired as paired_report

from helios_bench.assemble import build_cell, build_control
from helios_bench.gate import FloorSource, evaluate_paired, gate_report
from helios_bench.render import render_gate
from helios_bench.report import (
    InstructionSampling,
    Iteration,
    Report,
    Side,
    WorkloadClass,
    WorkloadResult,
    load_report,
    save_report,
)
from helios_bench.sources import RawCell, RawSide

FIXTURE = Path(__file__).parent / "fixtures" / "aa-run-38940834451.json"
REBUILD_PROFILE = "target/pgo-baseline-rebuild/helios-kernel.profdata"


def launches(rows: list) -> list[Iteration]:
    return [
        Iteration(index=index, elapsed_ms=elapsed, cold=index == 1, metrics=metrics)
        for index, (elapsed, metrics) in enumerate(rows, start=1)
    ]


def control_side(spec: dict) -> tuple[RawSide, RawSide]:
    def raw(values: list[float]) -> RawSide:
        return RawSide(
            run={"type": "run"},
            cells={"quickjs-loop": RawCell(iterations=launches([[value, {}] for value in values]))},
        )

    return raw(spec["before"]), raw(spec["after"])


def aa_report(swapped: bool, rebuild: bool, first_pass: bool, template: Report) -> Report:
    """Run 38940834451 as a paired report.

    `swapped` reads the empty commit as the baseline and dev as the
    candidate, which turns every improvement the run reported into a
    regression. `rebuild` adds the synthetic rebuild column: in an A/A
    run the candidate's column is itself an independently profiled
    second build of the baseline's code, so the rebuild is that other
    build — its other boot where the run booted it twice (the
    reconfirmed `fs-smallfiles`), its one boot elsewhere. `first_pass`
    keeps the reconfirm's first pair beside the second, as the runner now
    does.
    """
    fixture = json.loads(FIXTURE.read_text(encoding="utf-8"))
    baseline_side, candidate_side = (
        ("helios", "helios_baseline") if swapped else ("helios_baseline", "helios")
    )
    control = build_control(
        "quickjs-loop",
        {
            Side.HELIOS: control_side(fixture["control"][candidate_side]),
            Side.HELIOS_BASELINE: control_side(fixture["control"][baseline_side]),
        },
        THRESHOLDS,
    )
    workloads = []
    for workload in fixture["workloads"]:
        cells = {
            Side.HELIOS: build_cell(Side.HELIOS, launches(workload["cells"][candidate_side]), THRESHOLDS),
            Side.HELIOS_BASELINE: build_cell(
                Side.HELIOS_BASELINE, launches(workload["cells"][baseline_side]), THRESHOLDS
            ),
        }
        first = workload["first_pass"]
        if rebuild:
            other = first.get(candidate_side) or workload["cells"][candidate_side]
            cells[Side.HELIOS_BASELINE_REBUILD] = build_cell(
                Side.HELIOS_BASELINE_REBUILD, launches(other), THRESHOLDS
            )
        result = WorkloadResult(
            name=workload["name"],
            workload_class=WorkloadClass(workload["class"]),
            headline=workload["headline"],
            description=workload["name"],
            throughput_bytes=None,
            cells=cells,
            comparisons=[],
            parity_bug=False,
        )
        if first_pass and first:
            result.first_pass = {
                Side.HELIOS: build_cell(Side.HELIOS, launches(first[candidate_side]), THRESHOLDS),
                Side.HELIOS_BASELINE: build_cell(
                    Side.HELIOS_BASELINE, launches(first[baseline_side]), THRESHOLDS
                ),
            }
        workloads.append(result)
    run = template.run.model_copy(
        update={
            "id": fixture["run"],
            "reconfirmed": ["fs-smallfiles"],
            "baseline_rebuild_kernel_profile": REBUILD_PROFILE if rebuild else None,
        }
    )
    return template.model_copy(update={"run": run, "control": control, "workloads": workloads})


@pytest.fixture
def template(paired_flat_report: Report) -> Report:
    return paired_flat_report


def significant(report: Report) -> dict[tuple[str, str], str]:
    result = evaluate_paired(report)
    return {
        (row.workload, row.measurement): "regression" if row.regression else "improvement"
        for row in result.rows
        if row.regression or row.improvement
    }


def test_todays_gate_blocks_identical_code(template: Report) -> None:
    """The hazard: read with the empty commit as the baseline, run
    38940834451's A/A pair blocks on four headline workloads, through
    the gate as it stood — no rebuild column, no first pair."""
    result = evaluate_paired(aa_report(swapped=True, rebuild=False, first_pass=False, template=template))
    assert result.blocking
    assert {row.workload for row in result.headline_regressions} == {
        "fs-smallfiles",
        "instance-startup-100",
        "pipe-pingpong",
        "sched-tasks",
    }
    assert all(row.floor_source is FloorSource.CONTROL for row in result.rows if row.relative_floor)


def test_the_first_pair_puts_fs_smallfiles_on_its_boot_drift(template: Report) -> None:
    """The run's own reconfirm: its baseline image put `fs-smallfiles` at
    193 ms on one boot and 233 ms on the next. Kept beside the second
    pair, that 21% is the row's floor, and neither -11.2% nor its mirror
    image clears it."""
    for swapped in (False, True):
        report = aa_report(swapped=swapped, rebuild=False, first_pass=True, template=template)
        row = next(row for row in evaluate_paired(report).rows if row.workload == "fs-smallfiles")
        assert row.floor_source is FloorSource.BOOT
        assert row.boot_drift == pytest.approx(0.21, abs=0.01)
        assert row.ci_disjoint and not row.beyond_noise
        assert not row.regression and not row.improvement


@pytest.mark.parametrize("swapped", [False, True])
def test_the_aa_replay_with_a_rebuild_draws_no_verdict(template: Report, swapped: bool, tmp_path) -> None:
    """Run 38940834451 with its synthetic rebuild column and its first
    pair: on identical code no row is a regression or an improvement, in
    either reading, and each row that did take a verdict names the floor
    that turned it away."""
    report = aa_report(swapped=swapped, rebuild=True, first_pass=True, template=template)
    # What the gate reads is what a report written and read back holds.
    path = tmp_path / "report.json"
    save_report(report, path)
    report = load_report(path)
    assert report.run.rebuilt
    assert set(report.workload("fs-smallfiles").first_pass) == {Side.HELIOS, Side.HELIOS_BASELINE}

    result = evaluate_paired(report)
    assert not result.blocking
    assert significant(report) == {}
    before = significant(aa_report(swapped=swapped, rebuild=False, first_pass=False, template=template))
    assert before, "the run drew verdicts on identical code before the change"
    rows = {(row.workload, row.measurement): row for row in result.rows}
    for key in before:
        row = rows[key]
        assert row.within_build_drift or row.floor_source is FloorSource.BOOT, key
    text = render_gate(gate_report(report, None), report.run.lane)
    assert "baseline rebuild" in text and REBUILD_PROFILE in text
    assert "| Workload | Measurement | Baseline | Rebuild | Candidate |" in text
    assert "within build drift" in text
    assert "No significant regression or improvement" in text


def with_rebuild(report: Report, centers: dict[str, float], seed: int) -> Report:
    """`report` with a rebuild column timed at ``centers``."""

    rebuilt = report.model_copy(deep=True)
    rebuilt.run.baseline_rebuild_kernel_profile = REBUILD_PROFILE
    for workload in rebuilt.workloads:
        if workload.name not in centers:
            continue
        center = centers[workload.name]
        workload.cells[Side.HELIOS_BASELINE_REBUILD] = build_cell(
            Side.HELIOS_BASELINE_REBUILD,
            iterations(center, center * 0.02, seed + sum(map(ord, workload.name)) % 100, cold_extra=center),
            THRESHOLDS,
        )
    return rebuilt


def test_a_tight_workload_keeps_todays_sensitivity() -> None:
    """Two builds that agree change nothing: a 5% regression on a 1-2%
    control floor blocks on the control floor, exactly as it does in a
    run without a rebuild column. The rebuild here is the baseline's own
    launches moved 0.5%, inside the floor."""
    plain = paired_report(candidate=21.0, baseline=20.0, run_id="4790", seed=3)
    rebuilt = with_rebuild(
        plain, {"hostcall-loop": 20.1, "quickjs-loop": 100.5, "fs-smallfiles": 25.0}, seed=3
    )
    before = {(row.workload, row.measurement): row for row in evaluate_paired(plain).rows}
    after = evaluate_paired(rebuilt)
    row = next(
        row for row in after.rows if (row.workload, row.measurement) == ("hostcall-loop", "elapsed_ms")
    )
    assert row.build_drift is not None and row.build_drift < row.floor
    assert row.floor_source is FloorSource.CONTROL
    assert row.floor == pytest.approx(after.noise_floor)
    assert row.regression and after.blocking
    for current in after.rows:
        old = before[current.workload, current.measurement]
        assert (current.regression, current.improvement, current.floor) == (
            old.regression,
            old.improvement,
            old.floor,
        ), "a row whose builds agree is judged as it was without them"


def test_a_row_whose_builds_drift_wider_than_its_shift_does_not_block() -> None:
    """The baseline's second build sits 8% above its first; the candidate
    sits 5% above the first and below the second. The control floor alone
    calls that a regression; against the build drift it is noise, the
    row says so, and nothing blocks."""
    plain = paired_report(candidate=21.0, baseline=20.0, run_id="4791", seed=3)
    assert evaluate_paired(plain).blocking, "today's gate blocks it"
    rebuilt = with_rebuild(
        plain, {"hostcall-loop": 21.6, "quickjs-loop": 100.5, "fs-smallfiles": 25.0}, seed=4
    )
    result = evaluate_paired(rebuilt)
    row = next(
        row for row in result.rows if (row.workload, row.measurement) == ("hostcall-loop", "elapsed_ms")
    )
    assert row.floor_source is FloorSource.BUILD
    assert row.floor == pytest.approx(row.build_drift)
    assert row.build_drift == pytest.approx(0.08, abs=0.02)
    assert row.ci_disjoint and not row.beyond_noise
    assert row.within_build_drift and not row.regression
    assert not result.blocking
    text = render_gate(gate_report(rebuilt, None), rebuilt.run.lane)
    assert "within build drift" in text
    assert "build |" in text, "the row names the floor it was judged against"


def test_a_shift_past_the_build_drift_from_both_builds_still_blocks() -> None:
    """A drifting row is held to its drift, not excused by it: 20% past a
    baseline whose two builds sit 5% apart is a regression against both."""
    plain = paired_report(candidate=24.0, baseline=20.0, run_id="4792", seed=3)
    rebuilt = with_rebuild(
        plain, {"hostcall-loop": 21.0, "quickjs-loop": 100.5, "fs-smallfiles": 25.0}, seed=4
    )
    result = evaluate_paired(rebuilt)
    row = next(
        row for row in result.rows if (row.workload, row.measurement) == ("hostcall-loop", "elapsed_ms")
    )
    assert row.floor_source is FloorSource.BUILD
    assert row.regression and not row.within_build_drift
    assert result.blocking


def test_a_rebuilt_run_without_a_rebuild_cell_is_incomplete() -> None:
    """A headline workload with no second build has no build drift to be
    judged against: a missing column, which blocks like a missing baseline."""
    plain = paired_report(candidate=20.0, baseline=20.0, run_id="4793", seed=3)
    rebuilt = with_rebuild(plain, {"quickjs-loop": 100.5, "fs-smallfiles": 25.0}, seed=4)
    result = evaluate_paired(rebuilt)
    assert result.incomplete_headlines == ["hostcall-loop"]
    assert result.blocking


def test_a_rebuild_column_sampled_unlike_the_others_is_refused() -> None:

    plain = paired_report(candidate=20.0, baseline=20.0, run_id="4794", seed=3)
    rebuilt = with_rebuild(
        plain, {"hostcall-loop": 20.0, "quickjs-loop": 100.5, "fs-smallfiles": 25.0}, seed=4
    )
    rebuilt.run.baseline_rebuild_instruction_sampling = InstructionSampling(
        period_nanos=100_000, boots=3, samples=10, dropped=0
    )
    with pytest.raises(SystemExit, match="rebuild column"):
        evaluate_paired(rebuilt)
