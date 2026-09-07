import pytest

from helios_bench.gate import evaluate, evaluate_paired, gate_report
from helios_bench.render import render_gate
from helios_bench.report import Report, Side
from helios_bench.stats import StatsConfig, series_stats


def test_no_regression_between_identical_distributions(baseline_report: Report) -> None:
    result = evaluate(baseline_report, baseline_report)
    assert result.enforced
    assert not result.blocking
    assert all(not row.regression for row in result.rows)


@pytest.mark.parametrize("shift", [-6.0, 6.0])
def test_overlapping_intervals_do_not_claim_all_shifts_are_within_noise(
    paired_flat_report: Report, shift: float
) -> None:
    workload = paired_flat_report.workload("hostcall-loop")
    config = StatsConfig(**paired_flat_report.thresholds.model_dump(exclude={"iterations", "warmup_discard"}))
    samples = [90.0, 91.0, 92.0, 95.0, 100.0, 100.0, 105.0, 108.0, 109.0, 110.0]
    workload.cells[Side.HELIOS_BASELINE].warm = series_stats(samples, config)
    workload.cells[Side.HELIOS].warm = series_stats([value + shift for value in samples], config)
    assert all(cell.warm.cv < config.cv_bound for cell in workload.cells.values())
    result = evaluate_paired(paired_flat_report)
    row = next(row for row in result.rows if row.workload == workload.name)
    assert row.beyond_noise and not row.ci_disjoint
    assert not row.regression and not row.improvement and not result.blocking
    rendered = render_gate(gate_report(paired_flat_report, None), paired_flat_report.run.lane)
    assert "nothing moved beyond the noise floor" not in rendered
    assert "No significant regression or improvement" in rendered


def test_significant_headline_regression_blocks(baseline_report: Report, regressed_report: Report) -> None:
    result = evaluate(baseline_report, regressed_report)
    rows = {row.workload: row for row in result.rows}
    assert rows["hostcall-loop"].regression
    assert rows["hostcall-loop"].ci_disjoint and rows["hostcall-loop"].beyond_noise
    assert rows["hostcall-loop"].shift == pytest.approx(0.5, abs=0.1)
    assert not rows["quickjs-loop"].regression
    assert result.blocking
    assert [row.workload for row in result.headline_regressions] == ["hostcall-loop"]


def test_advisory_reports_never_block(baseline_report: Report, advisory_report: Report) -> None:
    result = evaluate(baseline_report, advisory_report)
    assert not result.enforced
    assert not result.blocking
    assert any(row.regression for row in result.rows)


def test_rejected_cells_are_skipped(baseline_report: Report, regressed_report: Report) -> None:
    cell = regressed_report.workload("hostcall-loop").cells[Side.HELIOS]
    rejected = cell.model_copy(update={"rejected": True, "rejection_reason": "test"})
    regressed_report.workload("hostcall-loop").cells[Side.HELIOS] = rejected
    result = evaluate(baseline_report, regressed_report)
    assert "hostcall-loop" not in {row.workload for row in result.rows}
    assert not result.blocking


def test_lane_mismatch_is_refused(baseline_report: Report) -> None:
    """Two lanes are two machines, whatever the manifest ships today."""
    other = baseline_report.model_copy(deep=True)
    other.run.lane = "aarch64-hvf"
    with pytest.raises(SystemExit):
        evaluate(baseline_report, other)


@pytest.mark.parametrize("side", [Side.HELIOS, Side.HELIOS_BASELINE])
@pytest.mark.parametrize("rejected", [False, True])
def test_incomplete_headline_pair_blocks(paired_flat_report: Report, side: Side, rejected: bool) -> None:
    workload = paired_flat_report.workload("hostcall-loop")
    if rejected:
        workload.cells[side] = workload.cells[side].model_copy(
            update={"rejected": True, "rejection_reason": "excessive variance"}
        )
    else:
        workload.cells.pop(side)
    result = evaluate_paired(paired_flat_report)
    assert result.blocking
    assert result.incomplete_headlines == ["hostcall-loop"]
    assert "quickjs-loop" in {row.workload for row in result.rows}
    rendered = render_gate(gate_report(paired_flat_report, None), paired_flat_report.run.lane)
    assert "incomplete paired evidence" in rendered
    assert "hostcall-loop" in rendered


def test_incomplete_nonheadline_pair_does_not_block(paired_flat_report: Report) -> None:
    workload = paired_flat_report.workload("hostcall-loop")
    workload.headline = False
    workload.cells.pop(Side.HELIOS_BASELINE)
    result = evaluate_paired(paired_flat_report)
    assert not result.blocking
    assert result.incomplete_headlines == []
