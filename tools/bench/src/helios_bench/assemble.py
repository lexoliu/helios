"""Turns raw iterations into a report: statistics, comparisons, verdicts."""

from __future__ import annotations

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
from helios_bench.sources import RawSide
from helios_bench.stats import (
    StatsConfig,
    intervals_overlap,
    noise_floor,
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


def build_cell(side: Side, iterations: list[Iteration], thresholds: Thresholds) -> Cell:
    config = stats_config(thresholds)
    ordered = sorted(iterations, key=lambda iteration: iteration.index)
    cold_values, warm_values = split_cold_warm([it.elapsed_ms for it in ordered], thresholds.warmup_discard)
    warm = series_stats(warm_values, config)
    cold = series_stats(cold_values, config)
    rejected = warm.cv > thresholds.cv_bound
    reason = f"warm CV {warm.cv:.3f} exceeds bound {thresholds.cv_bound:.3f}" if rejected else None
    metric_names = sorted({name for it in ordered[thresholds.warmup_discard :] for name in it.metrics})
    metrics = {}
    for name in metric_names:
        values = [it.metrics[name] for it in ordered[thresholds.warmup_discard :] if name in it.metrics]
        metrics[name] = series_stats(values, config)
    return Cell(
        side=side,
        iterations=ordered,
        cold=cold,
        warm=warm,
        rejected=rejected,
        rejection_reason=reason,
        metrics=metrics,
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
                and workload["class"] == WorkloadClass.COMPUTE
                and comparison.speedup < 1.0
                and comparison.significant
                and comparison.beyond_noise
            ):
                parity_bug = True
    return WorkloadResult(
        name=workload["name"],
        workload_class=WorkloadClass(workload["class"]),
        headline=bool(workload.get("headline", False)),
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
) -> Report:
    floor = control.noise_floor if control is not None else 0.0
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
            cells[side] = build_cell(side, raw_cell.iterations, thresholds)
        if not cells and not failures:
            continue
        results.append(build_workload(workload, cells, floor, failures))
    return Report(
        run=run,
        hardware=hardware,
        pins=pins,
        thresholds=thresholds,
        control=control,
        workloads=results,
    )
