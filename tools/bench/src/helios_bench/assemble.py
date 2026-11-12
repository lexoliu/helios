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
from helios_bench.sources import RawCell, RawSide
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
    uncompared = {Side(side): reason for side, reason in workload.get("uncompared", {}).items()}
    helios = cells.get(Side.HELIOS)
    if helios is not None and not helios.rejected:
        for side in (Side.LINUX_WASMTIME, Side.LINUX_NATIVE):
            other = cells.get(side)
            # An uncompared side that was measured is a reference shown
            # beside the Helios number, never a speedup against it.
            if other is None or other.rejected or side in uncompared:
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
        uncompared=uncompared,
        description=workload["description"],
        throughput_bytes=workload.get("throughput_bytes"),
        cells=cells,
        failures=failures or {},
        comparisons=comparisons,
        parity_bug=parity_bug,
    )


#: The sides a workload's `matched_metrics` hold equal: the Helios column and
#: the Linux column that runs the same wasm. A baseline image is driven by its
#: own commit's tooling, which may predate the metric.
MATCHED_SIDES = (Side.HELIOS, Side.LINUX_WASMTIME)


def check_matched_metrics(workload: dict, cells: dict[Side, Cell]) -> None:
    """Stops assembly when the sides a comparison holds a metric fixed for
    ran with different values of it, or one of them did not report it.

    `aot-curl` compares one compiler on two systems only at one worker
    count; a cell that compiled with another count is a different
    measurement, and the run's raw JSONL says which.
    """
    for name in workload.get("matched_metrics", []):
        values: dict[Side, set[float]] = {}
        for side in MATCHED_SIDES:
            cell = cells.get(side)
            if cell is None:
                continue
            missing = [it.index for it in cell.iterations if name not in it.metrics]
            if missing:
                raise SystemExit(
                    f"workload {workload['name']}: {side} iterations {missing} do not report "
                    f"`{name}`, which the manifest holds equal across {', '.join(MATCHED_SIDES)}"
                )
            values[side] = {it.metrics[name] for it in cell.iterations}
        if len(set().union(*values.values())) > 1:
            seen = "; ".join(f"{side}: {sorted(found)}" for side, found in values.items())
            raise SystemExit(
                f"workload {workload['name']}: `{name}` must be one value across "
                f"{', '.join(MATCHED_SIDES)}, found {seen}"
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
        check_matched_metrics(workload, cells)
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
