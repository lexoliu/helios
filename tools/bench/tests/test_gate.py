import pytest

from helios_bench.gate import (
    DURATION,
    FOOTPRINT,
    RATE,
    Column,
    Unit,
    UnpairedMetric,
    evaluate,
    evaluate_paired,
    gate_report,
    metric_unit,
)
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
    rows = {(row.workload, row.measurement): row for row in result.rows}
    assert rows["hostcall-loop", "elapsed_ms"].regression
    assert rows["hostcall-loop", "elapsed_ms"].ci_disjoint
    assert rows["hostcall-loop", "elapsed_ms"].beyond_noise
    assert rows["hostcall-loop", "elapsed_ms"].shift == pytest.approx(0.5, abs=0.1)
    assert not rows["quickjs-loop", "elapsed_ms"].regression
    assert result.blocking
    assert {row.workload for row in result.headline_regressions} == {"hostcall-loop"}


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


def rewrite_metric(report: Report, workload_name: str, metric: str, samples: list[float]) -> None:
    """Replaces one metric's candidate-side warm statistics."""
    config = StatsConfig(**report.thresholds.model_dump(exclude={"iterations", "warmup_discard"}))
    report.workload(workload_name).cells[Side.HELIOS].metrics[metric] = series_stats(samples, config)


def test_a_metric_the_wall_clock_hides_still_blocks(paired_flat_report: Report) -> None:
    """The point of the whole thing (#279).

    `procbench` times its teardown separately because the wall clock of
    the round trip averages it away. A gate that compares the wall clock
    alone puts it straight back.
    """
    baseline = paired_flat_report.workload("hostcall-loop").cells[Side.HELIOS_BASELINE]
    rewrite_metric(
        paired_flat_report,
        "hostcall-loop",
        "rtt_p50_us",
        [baseline.metrics["rtt_p50_us"].median * 1.5 + offset for offset in (-0.4, 0.0, 0.4, 0.2, -0.2)],
    )
    result = evaluate_paired(paired_flat_report)
    rows = {(row.workload, row.measurement): row for row in result.rows}

    assert not rows["hostcall-loop", "elapsed_ms"].regression
    assert rows["hostcall-loop", "rtt_p50_us"].regression
    assert result.blocking
    assert [row.measurement for row in result.headline_regressions] == ["rtt_p50_us"]

    rendered = render_gate(gate_report(paired_flat_report, None), paired_flat_report.run.lane)
    assert "`hostcall-loop`/`rtt_p50_us` regressed significantly" in rendered
    assert "| `rtt_p50_us` |" in rendered


def test_a_dispersed_metric_is_rejected_rather_than_judged(paired_flat_report: Report) -> None:
    baseline = paired_flat_report.workload("hostcall-loop").cells[Side.HELIOS_BASELINE]
    center = baseline.metrics["rtt_p50_us"].median
    rewrite_metric(
        paired_flat_report,
        "hostcall-loop",
        "rtt_p50_us",
        [center * factor for factor in (0.4, 0.8, 1.5, 2.2, 3.0)],
    )
    result = evaluate_paired(paired_flat_report)
    row = next(row for row in result.rows if row.measurement == "rtt_p50_us")

    assert row.rejected
    assert "coefficient of variation" in row.rejection_reason
    assert not row.regression and not row.improvement
    assert not result.blocking
    assert row in result.rejected_rows
    assert "rejected: warm coefficient of variation" in render_gate(
        gate_report(paired_flat_report, None), paired_flat_report.run.lane
    )


def test_a_footprint_that_did_not_move_is_not_a_regression(paired_flat_report: Report) -> None:
    """The floor does not apply, so nothing else may stand in for it.

    Two images built from the same kernel source report the same
    footprint, and a rule that judges a footprint without a noise floor
    has to say "unchanged" for that case or it blocks every tooling
    change.
    """
    config = StatsConfig(**paired_flat_report.thresholds.model_dump(exclude={"iterations", "warmup_discard"}))
    cells = paired_flat_report.workload("hostcall-loop").cells
    for side in (Side.HELIOS_BASELINE, Side.HELIOS):
        cells[side].metrics["memory_per_instance_bytes"] = series_stats([9_853_797.0] * 5, config)
    result = evaluate_paired(paired_flat_report)
    row = next(row for row in result.rows if row.measurement == "memory_per_instance_bytes")

    assert row.shift == 0.0
    assert not row.beyond_noise
    assert not row.regression and not row.improvement
    assert not result.blocking


def test_a_metric_only_one_column_measured_is_named_and_blocks_nothing(
    paired_flat_report: Report,
) -> None:
    """A change that adds a metric must not fail on its own first run."""
    cells = paired_flat_report.workload("hostcall-loop").cells
    cells[Side.HELIOS_BASELINE].metrics.pop("switches_per_s")
    result = evaluate_paired(paired_flat_report)

    assert "switches_per_s" not in {row.measurement for row in result.rows if row.workload == "hostcall-loop"}
    assert result.unpaired_metrics == [
        UnpairedMetric(workload="hostcall-loop", metric="switches_per_s", measured_by=Column.CANDIDATE)
    ]
    assert not result.blocking
    assert "`hostcall-loop`/`switches_per_s` (candidate only)" in render_gate(
        gate_report(paired_flat_report, None), paired_flat_report.run.lane
    )


def test_a_metric_without_a_unit_stops_the_gate(paired_flat_report: Report) -> None:
    """The name is the only declaration either harness gets."""
    cells = paired_flat_report.workload("hostcall-loop").cells
    for side in (Side.HELIOS, Side.HELIOS_BASELINE):
        cells[side].metrics["throughput"] = cells[side].metrics["rtt_p50_us"]
    with pytest.raises(SystemExit, match="ends in no unit"):
        evaluate_paired(paired_flat_report)


@pytest.mark.parametrize(
    ("metric", "expected"),
    [
        ("mib_per_s", RATE),
        ("switches_per_s", RATE),
        ("ns_per_call", DURATION),
        ("memory_per_instance_bytes", FOOTPRINT),
        ("teardown_ms", DURATION),
        ("first_output_p99_us", DURATION),
    ],
)
def test_every_metric_in_the_tree_reads_its_unit(metric: str, expected: Unit) -> None:
    assert metric_unit(metric) == expected


def test_a_footprint_is_not_held_to_a_timing_floor(paired_flat_report: Report) -> None:
    """A machine that drifts does not make an instance bigger.

    The noise floor is the control workload's drift, so it bounds
    durations. `memory_per_instance_bytes` repeats exactly, and holding
    it to a timing floor would let a repeatable footprint regression
    through under a floor it can never reach.
    """
    config = StatsConfig(**paired_flat_report.thresholds.model_dump(exclude={"iterations", "warmup_discard"}))
    cells = paired_flat_report.workload("hostcall-loop").cells
    cells[Side.HELIOS_BASELINE].metrics["memory_per_instance_bytes"] = series_stats([9_853_797.0] * 5, config)
    cells[Side.HELIOS].metrics["memory_per_instance_bytes"] = series_stats([9_895_373.0] * 5, config)
    result = evaluate_paired(paired_flat_report)
    row = next(row for row in result.rows if row.measurement == "memory_per_instance_bytes")

    assert row.shift == pytest.approx(0.0042, abs=0.0005)
    assert row.shift < result.noise_floor
    assert row.ci_disjoint and row.beyond_noise and row.regression
    assert result.blocking


def test_an_extremum_never_blocks_however_it_behaves(paired_flat_report: Report) -> None:
    """Run 34223160269: two identical kernels, `first_output_max_us` +14.1%.

    The max of a hundred-way concurrent spawn is the tail of a queue, and
    no sample size makes it attributable to a change (#286).
    """
    cells = paired_flat_report.workload("hostcall-loop").cells
    config = StatsConfig(**paired_flat_report.thresholds.model_dump(exclude={"iterations", "warmup_discard"}))
    for side, center in ((Side.HELIOS_BASELINE, 90_472.0), (Side.HELIOS, 103_192.0)):
        cells[side].metrics["first_output_max_us"] = series_stats(
            [center + offset for offset in (-40.0, 0.0, 40.0, 20.0, -20.0)], config
        )
        cells[side].metrics["first_output_samples"] = series_stats([100.0] * 5, config)
    result = evaluate_paired(paired_flat_report)
    row = next(row for row in result.rows if row.measurement == "first_output_max_us")

    assert row.shift == pytest.approx(0.141, abs=0.001)
    assert row.ci_disjoint and row.beyond_noise
    assert row.diagnostic and not row.regression
    assert "extremum" in row.diagnostic_reason
    assert not result.blocking
    assert "| diagnostic |" in render_gate(gate_report(paired_flat_report, None), paired_flat_report.run.lane)


def test_a_percentile_blocks_only_when_the_samples_make_it_one(paired_flat_report: Report) -> None:
    config = StatsConfig(**paired_flat_report.thresholds.model_dump(exclude={"iterations", "warmup_discard"}))
    cells = paired_flat_report.workload("hostcall-loop").cells
    for side, center in ((Side.HELIOS_BASELINE, 1_000.0), (Side.HELIOS, 1_500.0)):
        cells[side].metrics["first_output_p99_us"] = series_stats(
            [center + offset for offset in (-4.0, 0.0, 4.0, 2.0, -2.0)], config
        )

    # A hundred samples make the nearest-rank p99 the second largest.
    for side in (Side.HELIOS_BASELINE, Side.HELIOS):
        cells[side].metrics["first_output_samples"] = series_stats([100.0] * 5, config)
    row = next(
        row for row in evaluate_paired(paired_flat_report).rows if row.measurement == "first_output_p99_us"
    )
    assert row.diagnostic and not row.regression
    assert "1,000 are needed" in row.diagnostic_reason

    # A thousand, and ten of them lie past the rank.
    for side in (Side.HELIOS_BASELINE, Side.HELIOS):
        cells[side].metrics["first_output_samples"] = series_stats([4096.0] * 5, config)
    result = evaluate_paired(paired_flat_report)
    row = next(row for row in result.rows if row.measurement == "first_output_p99_us")
    assert not row.diagnostic and row.regression
    assert result.blocking


def test_a_percentile_with_no_count_is_not_taken_on_trust(paired_flat_report: Report) -> None:
    """A report written before the harness counted its samples."""
    cells = paired_flat_report.workload("hostcall-loop").cells
    for side in (Side.HELIOS_BASELINE, Side.HELIOS):
        cells[side].metrics.pop("rtt_samples")
    row = next(row for row in evaluate_paired(paired_flat_report).rows if row.measurement == "rtt_p50_us")
    assert row.diagnostic
    assert "does not report" in row.diagnostic_reason
    # The sample count is context, never a row of its own.
    assert "rtt_samples" not in {row.measurement for row in evaluate_paired(paired_flat_report).rows}


def test_a_footprint_moves_in_pages(paired_flat_report: Report) -> None:
    """1,397 bytes on 9.86 MB is accounting; ten pages is a regression.

    Both numbers are real: the first is two identical kernels in run
    34223160269, the second is what the allocator cache of #169 cost per
    instance.
    """
    config = StatsConfig(**paired_flat_report.thresholds.model_dump(exclude={"iterations", "warmup_discard"}))
    cells = paired_flat_report.workload("hostcall-loop").cells
    cells[Side.HELIOS_BASELINE].metrics["memory_per_instance_bytes"] = series_stats([9_856_173.0] * 5, config)

    cells[Side.HELIOS].metrics["memory_per_instance_bytes"] = series_stats([9_854_776.0] * 5, config)
    row = next(
        row
        for row in evaluate_paired(paired_flat_report).rows
        if row.measurement == "memory_per_instance_bytes"
    )
    assert row.ci_disjoint and not row.beyond_noise
    assert not row.regression and not row.improvement

    cells[Side.HELIOS].metrics["memory_per_instance_bytes"] = series_stats([9_897_749.0] * 5, config)
    result = evaluate_paired(paired_flat_report)
    row = next(row for row in result.rows if row.measurement == "memory_per_instance_bytes")
    assert row.beyond_noise and row.regression
    assert result.blocking
