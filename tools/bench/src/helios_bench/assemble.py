"""Turns raw iterations into a report: statistics, comparisons, verdicts."""

from __future__ import annotations

import statistics

from helios_bench.report import (
    Cell,
    Comparison,
    Control,
    ControlSide,
    Hardware,
    Iteration,
    Pins,
    Report,
    RunInfo,
    Side,
    Thresholds,
    WorkloadClass,
    WorkloadResult,
)
from helios_bench.sources import RawCell, RawSide
from helios_bench.stats import (
    StatsConfig,
    intervals_overlap,
    noise_floor,
    relative_shift,
    series_stats,
    split_cold_warm,
)


def stats_config(thresholds: Thresholds) -> StatsConfig:
    return StatsConfig(
        bootstrap_resamples=thresholds.bootstrap_resamples,
        confidence=thresholds.confidence,
        bootstrap_seed=thresholds.bootstrap_seed,
        cv_bound=thresholds.cv_bound,
    )


def build_cell(
    side: Side, iterations: list[Iteration], thresholds: Thresholds, round_floor: float | None = None
) -> Cell:
    """One cell's statistics, pooling the alternating rounds it was timed in (#491).

    Each round's first ``warmup_discard`` launches are its cold series and
    the rest its warm one; the cell's warm series pools every round's. A
    cell timed in more than one round is also rejected, like a dispersed
    one, when its rounds' warm medians spread by more than ``round_floor``,
    the noise floor: two orders of the same run that disagree by more than
    the host moved by say the order, not the system, set the number.
    """
    config = stats_config(thresholds)
    clocks = {iteration.clock for iteration in iterations}
    if len(clocks) > 1:
        raise ValueError(
            f"a {side} cell pools iterations timed with different clocks "
            f"({sorted(str(clock) for clock in clocks)}), which are not one series"
        )
    ordered = sorted(iterations, key=lambda iteration: (iteration.round, iteration.index))
    by_round: dict[int, list[Iteration]] = {}
    for iteration in ordered:
        by_round.setdefault(iteration.round, []).append(iteration)
    cold_values: list[float] = []
    warm_values: list[float] = []
    round_warm: list[list[float]] = []
    for members in by_round.values():
        cold, warm = split_cold_warm([it.elapsed_ms for it in members], thresholds.warmup_discard)
        cold_values += cold
        warm_values += warm
        round_warm.append(warm)
    warm = series_stats(warm_values, config)
    cold = series_stats(cold_values, config)
    reasons = []
    if warm.cv > thresholds.cv_bound:
        reasons.append(f"warm CV {warm.cv:.3f} exceeds bound {thresholds.cv_bound:.3f}")
    round_medians: list[float] = []
    if len(by_round) > 1:
        if round_floor is None:
            raise ValueError(
                f"a cell timed in {len(by_round)} rounds is judged against the noise floor, "
                "and the run has no control workload to give one"
            )
        round_medians = [float(statistics.median(values)) for values in round_warm]
        disagreement = relative_shift(min(round_medians), max(round_medians))
        if disagreement > round_floor:
            reasons.append(
                f"round medians disagree by {disagreement:.3f}, beyond the noise floor {round_floor:.3f}"
            )
    metric_names = sorted(
        {
            name
            for members in by_round.values()
            for it in members[thresholds.warmup_discard :]
            for name in it.metrics
        }
    )
    metrics = {}
    for name in metric_names:
        values = [
            it.metrics[name]
            for members in by_round.values()
            for it in members[thresholds.warmup_discard :]
            if name in it.metrics
        ]
        metrics[name] = series_stats(values, config)
    return Cell(
        side=side,
        iterations=ordered,
        clock=next(iter(clocks), None),
        cold=cold,
        warm=warm,
        rejected=bool(reasons),
        rejection_reason="; ".join(reasons) if reasons else None,
        metrics=metrics,
        round_medians=round_medians,
    )


def compare(helios: Cell, other: Cell, floor: float) -> Comparison:
    speedup = other.warm.median / helios.warm.median
    return Comparison(
        against=other.side,
        speedup=speedup,
        significant=not intervals_overlap(helios.warm, other.warm),
        beyond_noise=abs(speedup - 1.0) > floor,
    )


def build_workload(
    workload: dict,
    cells: dict[Side, Cell],
    floor: float,
    failures: dict[Side, str] | None = None,
) -> WorkloadResult:
    comparisons = []
    parity_bug = False
    helios = cells.get(Side.HELIOS)
    if helios is not None and not helios.rejected:
        for side in (Side.LINUX_WASMTIME, Side.LINUX_NATIVE):
            other = cells.get(side)
            if other is None or other.rejected:
                continue
            comparison = compare(helios, other, floor)
            comparisons.append(comparison)
            if (
                side is Side.LINUX_WASMTIME
                and comparison.speedup < 1.0
                and comparison.significant
                and comparison.beyond_noise
            ):
                parity_bug = True
    return WorkloadResult(
        name=workload["name"],
        workload_class=WorkloadClass(workload["class"]),
        headline=bool(workload.get("headline", False)),
        uncompared={Side(side): reason for side, reason in workload.get("uncompared", {}).items()},
        description=workload["description"],
        throughput_bytes=workload.get("throughput_bytes"),
        cells=cells,
        failures=failures or {},
        comparisons=comparisons,
        parity_bug=parity_bug,
    )


def build_control(
    workload_name: str,
    sides: dict[Side, tuple[RawSide, RawSide]],
    thresholds: Thresholds,
) -> Control | None:
    if not sides:
        return None
    control_sides = {}
    for side, (before, after) in sides.items():
        before_cell = build_cell(side, before.cells[workload_name].iterations, thresholds)
        after_cell = build_cell(side, after.cells[workload_name].iterations, thresholds)
        floor = noise_floor(before_cell.warm, after_cell.warm)
        control_sides[side] = ControlSide(before=before_cell.warm, after=after_cell.warm, noise_floor=floor)
    return Control(
        workload=workload_name,
        sides=control_sides,
        noise_floor=max(side.noise_floor for side in control_sides.values()),
    )


def assemble_report(
    workloads: list[dict],
    sides: dict[Side, RawSide],
    control: Control | None,
    run: RunInfo,
    hardware: Hardware,
    pins: Pins,
    thresholds: Thresholds,
    first_pass: dict[Side, dict[str, RawCell]] | None = None,
) -> Report:
    """The report of one pass of the suite.

    ``first_pass`` holds, per Helios image, the cells of the first pair
    of boots of each workload the reconfirm measured again; ``sides``
    already carries the second pair. A first-pass cell that failed was
    never compared, so it is not one.
    """
    floor = control.noise_floor if control is not None else 0.0
    round_floor = control.noise_floor if control is not None else None
    results = []
    for workload in workloads:
        cells: dict[Side, Cell] = {}
        failures: dict[Side, str] = {}
        for side, raw in sides.items():
            raw_cell = raw.cells.get(workload["name"])
            if raw_cell is None:
                continue
            if raw_cell.failure is not None:
                failures[side] = raw_cell.failure
                continue
            cells[side] = build_cell(side, raw_cell.iterations, thresholds, round_floor)
        if not cells and not failures:
            continue
        result = build_workload(workload, cells, floor, failures)
        for side, raw_cells in (first_pass or {}).items():
            raw_cell = raw_cells.get(workload["name"])
            if raw_cell is not None and raw_cell.failure is None:
                result.first_pass[side] = build_cell(side, raw_cell.iterations, thresholds)
        results.append(result)
    return Report(
        run=run,
        hardware=hardware,
        pins=pins,
        thresholds=thresholds,
        control=control,
        workloads=results,
    )
