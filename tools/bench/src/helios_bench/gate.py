"""Regression gate: what a candidate report has to survive.

Two comparisons, and the difference between them is the whole point:

- **Paired.** The candidate against the `helios_baseline` side of its own
  report — two images timed on one host, in one job, minutes apart. There
  is no change of machine between the columns, so the comparison is
  enforced whether or not the report is publishable.
- **Cross-run.** The candidate against the newest `dev` report of the
  same lane, taken in another job on another runner. A shared runner does
  not pin the CPU model, so this one is only as good as the machines
  happened to be alike: it enforces when both reports are publishable
  **and** name the same host CPU, and otherwise says what it saw.

Either comparison judges every measurement a cell carries, not only its
wall clock: the host-side `elapsed_ms` of the whole round trip, and each
`bench.<name>` metric the workload printed from inside the guest. A
workload that times its own teardown separately (`procbench`, for the
kernel allocator) did so because the wall clock averages it away; the
gate would undo that by comparing the wall clock alone.
"""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass
from enum import StrEnum

from helios_bench.report import Cell, Report, SeriesStats, Side, WorkloadResult
from helios_bench.stats import intervals_overlap, relative_shift


class GateKind(StrEnum):
    PAIRED = "paired"
    CROSS_RUN = "cross_run"


class Direction(StrEnum):
    """Which way a measurement has to move to be an improvement."""

    LOWER_IS_BETTER = "lower_is_better"
    HIGHER_IS_BETTER = "higher_is_better"


#: The wall clock of the whole round trip, as a row names it.
ELAPSED = "elapsed_ms"


@dataclass(frozen=True)
class Unit:
    """What a measurement's unit says about how to read a shift.

    `drifts_with_the_host` is what the noise floor is for. The floor is
    the control workload's drift between the run before the workloads and
    the run after, so it bounds how much the *machine* moved: it applies
    to a duration and to a rate of durations, and to nothing else. A
    footprint in bytes does not get slower when the host is busy, so
    holding it to a timing floor would let a repeatable regression
    through — the bootstrap intervals are the whole test for it.
    """

    direction: Direction
    drifts_with_the_host: bool


DURATION = Unit(Direction.LOWER_IS_BETTER, drifts_with_the_host=True)
RATE = Unit(Direction.HIGHER_IS_BETTER, drifts_with_the_host=True)
FOOTPRINT = Unit(Direction.LOWER_IS_BETTER, drifts_with_the_host=False)

#: Unit suffix -> unit, longest suffix first so `_per_s` is read as a rate
#: rather than as whatever shorter suffix it happens to end with.
#:
#: Every metric in the tree carries its unit in its name, because both
#: harnesses parse `bench.<name>=<number>` off a workload's stdout and the
#: name is all either of them gets. Reading the unit off the name is
#: therefore reading the only declaration there is, and a metric that
#: declares none stops the gate rather than being guessed at.
METRIC_UNITS: tuple[tuple[str, Unit], ...] = (
    ("_per_second", RATE),
    ("_per_call", DURATION),
    ("_per_op", DURATION),
    ("_per_s", RATE),
    ("_bytes", FOOTPRINT),
    ("_us", DURATION),
    ("_ms", DURATION),
    ("_ns", DURATION),
)


def metric_unit(name: str) -> Unit:
    for suffix, unit in METRIC_UNITS:
        if name.endswith(suffix):
            return unit
    raise SystemExit(
        f"metric `{name}` ends in no unit the gate knows, so it cannot tell an improvement "
        "from a regression; name it after its unit "
        f"({', '.join(suffix for suffix, _ in METRIC_UNITS)}) or add the unit to "
        "helios_bench.gate.METRIC_UNITS"
    )


GATE_TITLES = {
    GateKind.PAIRED: "Paired, one host, one job",
    GateKind.CROSS_RUN: "Cross-run, against the latest `dev` report",
}


@dataclass(frozen=True)
class GateRow:
    workload: str
    #: `elapsed_ms`, or the name of the `bench.<name>` metric this row is.
    measurement: str
    headline: bool
    direction: Direction
    baseline_median: float
    candidate_median: float
    shift: float
    ci_disjoint: bool
    beyond_noise: bool
    regression: bool
    improvement: bool
    #: Set when either side's warm series is too dispersed to compare. The
    #: row is printed with its reason and takes part in no verdict, the way
    #: a variance-rejected cell does.
    rejected: bool = False
    rejection_reason: str | None = None


class Column(StrEnum):
    """Which side of a comparison a value came from.

    Not `Side`: the two columns of a cross-run comparison are both
    `Side.HELIOS`, and what distinguishes them is which report they are
    in.
    """

    BASELINE = "baseline"
    CANDIDATE = "candidate"


@dataclass(frozen=True)
class UnpairedMetric:
    """A metric only one column of a comparison measured.

    A candidate that adds a metric, or a baseline old enough to predate
    one, has nothing to compare it against. That is reported rather than
    dropped, and it does not block: the first run of a new metric would
    otherwise fail the change that introduced it.
    """

    workload: str
    metric: str
    measured_by: Column


@dataclass(frozen=True)
class GateResult:
    kind: GateKind
    lane: str
    baseline_run: str | None
    candidate_run: str | None
    baseline_label: str
    candidate_label: str
    baseline_host: str
    candidate_host: str
    noise_floor: float
    rows: list[GateRow]
    incomplete_headlines: list[str]
    unpaired_metrics: list[UnpairedMetric]
    blocking: bool
    enforced: bool

    @property
    def regressions(self) -> list[GateRow]:
        return [row for row in self.rows if row.regression]

    @property
    def rejected_rows(self) -> list[GateRow]:
        return [row for row in self.rows if row.rejected]

    @property
    def improvements(self) -> list[GateRow]:
        return [row for row in self.rows if row.improvement]

    @property
    def headline_regressions(self) -> list[GateRow]:
        return [row for row in self.rows if row.regression and row.headline]


@dataclass(frozen=True)
class GateReport:
    """What the gate step prints and comments: the paired table first."""

    paired: GateResult | None
    cross_run: GateResult | None

    @property
    def results(self) -> list[GateResult]:
        return [result for result in (self.paired, self.cross_run) if result is not None]

    @property
    def blocking(self) -> bool:
        return any(result.blocking for result in self.results)


def compare_series(
    workload: WorkloadResult,
    measurement: str,
    before: SeriesStats,
    after: SeriesStats,
    floor: float,
    cv_bound: float,
) -> GateRow:
    """One row: two warm series of one measurement, judged the same way.

    Significant means the two bootstrap intervals of the medians are
    disjoint **and** the median moved by more than the floor the run's
    control measured. Which sign of movement is the bad one, and whether
    the floor applies at all, come from the measurement's unit.
    """
    unit = DURATION if measurement == ELAPSED else metric_unit(measurement)
    shift = relative_shift(before.median, after.median)
    disjoint = not intervals_overlap(before, after)
    beyond = abs(shift) > floor if unit.drifts_with_the_host else shift != 0.0
    worse = shift > 0 if unit.direction is Direction.LOWER_IS_BETTER else shift < 0
    dispersed = max(before.cv, after.cv)
    rejected = dispersed > cv_bound
    significant = disjoint and beyond and not rejected
    return GateRow(
        workload=workload.name,
        measurement=measurement,
        headline=workload.headline,
        direction=unit.direction,
        baseline_median=before.median,
        candidate_median=after.median,
        shift=shift,
        ci_disjoint=disjoint,
        beyond_noise=beyond,
        regression=significant and worse,
        improvement=significant and not worse,
        rejected=rejected,
        rejection_reason=(
            f"warm coefficient of variation {dispersed:.3f} exceeds the run's bound {cv_bound:.3f}"
            if rejected
            else None
        ),
    )


def gate_rows(
    pairs: Iterable[tuple[WorkloadResult, Cell, Cell]], floor: float, cv_bound: float
) -> tuple[list[GateRow], list[UnpairedMetric]]:
    """Every measurement of every comparable cell pair.

    The cell's own `elapsed_ms` first, then each `bench.<name>` metric
    both sides measured, in the order the report stores them. A metric
    only one side has cannot be compared and is reported separately.
    """
    rows: list[GateRow] = []
    unpaired: list[UnpairedMetric] = []
    for workload, base_cell, cand_cell in pairs:
        rows.append(compare_series(workload, ELAPSED, base_cell.warm, cand_cell.warm, floor, cv_bound))
        for metric in sorted(set(base_cell.metrics) | set(cand_cell.metrics)):
            before = base_cell.metrics.get(metric)
            after = cand_cell.metrics.get(metric)
            if before is None or after is None:
                unpaired.append(
                    UnpairedMetric(
                        workload=workload.name,
                        metric=metric,
                        measured_by=Column.CANDIDATE if before is None else Column.BASELINE,
                    )
                )
                continue
            if before.median <= 0:
                # `relative_shift` has no reference to divide by, and a
                # measurement that reads zero on the baseline is not a
                # measurement of anything the candidate can be worse at.
                continue
            rows.append(compare_series(workload, metric, before, after, floor, cv_bound))
    return rows, unpaired


def comparable(base_cell: Cell | None, cand_cell: Cell | None) -> bool:
    return (
        base_cell is not None and cand_cell is not None and not base_cell.rejected and not cand_cell.rejected
    )


def noise_floor(*reports: Report) -> float:
    return max((report.control.noise_floor if report.control else 0.0) for report in reports)


def short(sha: str | None) -> str:
    return sha[:12] if sha else "unknown"


def evaluate(baseline: Report, candidate: Report) -> GateResult:
    """The cross-run comparison: two runs, two jobs, two machines."""
    if baseline.run.lane != candidate.run.lane:
        raise SystemExit(
            "gate compares reports from one lane; "
            f"baseline is {baseline.run.lane}, candidate is {candidate.run.lane}"
        )
    floor = noise_floor(baseline, candidate)
    pairs = []
    for workload in candidate.workloads:
        before = baseline.workload(workload.name)
        if before is None:
            continue
        base_cell = before.cells.get(Side.HELIOS)
        cand_cell = workload.cells.get(Side.HELIOS)
        if not comparable(base_cell, cand_cell):
            continue
        pairs.append((workload, base_cell, cand_cell))
    rows, unpaired = gate_rows(pairs, floor, candidate.thresholds.cv_bound)
    # Two runs of one lane are two machines as often as they are one
    # machine twice, and the run record is where that is visible.
    enforced = (
        baseline.run.publishable
        and candidate.run.publishable
        and baseline.hardware.cpu == candidate.hardware.cpu
    )
    return GateResult(
        kind=GateKind.CROSS_RUN,
        lane=candidate.run.lane,
        baseline_run=baseline.run.id,
        candidate_run=candidate.run.id,
        baseline_label=f"run {baseline.run.id or 'local'}, Helios `{short(baseline.run.helios_git_sha)}`",
        candidate_label=f"run {candidate.run.id or 'local'}, Helios `{short(candidate.run.helios_git_sha)}`",
        baseline_host=baseline.hardware.cpu,
        candidate_host=candidate.hardware.cpu,
        noise_floor=floor,
        rows=rows,
        incomplete_headlines=[],
        unpaired_metrics=unpaired,
        blocking=enforced and any(row.regression and row.headline for row in rows),
        enforced=enforced,
    )


def image_label(sha: str | None, ref: str | None, build: str | None, other_build: str | None) -> str:
    """How one column of a paired table names its image.

    The commit always, then whatever distinguishes this image from the
    other one: the ref it was asked for, and the cargo profile its kernel
    was built with when the two differ — a PGO pairing varies the build
    and not the commit, so without that the two columns would carry the
    same label.
    """
    qualifiers = []
    if ref:
        qualifiers.append(ref)
    if build and other_build and build != other_build:
        qualifiers.append(build)
    label = f"`{short(sha)}`"
    return f"{label} ({', '.join(qualifiers)})" if qualifiers else label


def evaluate_paired(candidate: Report) -> GateResult | None:
    """The candidate against the baseline image of its own run.

    Enforced whenever it exists: both columns came out of one job on one
    host, so nothing about the machine can explain the difference and a
    headline regression is the change's own. A run that was asked to pair
    and produced no baseline cells is a failure, not a report without a
    column.
    """
    measured = Side.HELIOS_BASELINE in candidate.measured_sides()
    if not candidate.run.paired:
        if measured:
            raise SystemExit(
                "the report carries a helios_baseline side but no baseline commit; "
                "its run record cannot say what the column was built from"
            )
        return None
    if not measured:
        raise SystemExit(
            f"the run paired against {short(candidate.run.baseline_git_sha)} but measured no "
            "helios_baseline cell; a paired run without its baseline column is a failed run"
        )
    floor = noise_floor(candidate)
    pairs = []
    incomplete_headlines = []
    for workload in candidate.workloads:
        base_cell = workload.cells.get(Side.HELIOS_BASELINE)
        cand_cell = workload.cells.get(Side.HELIOS)
        if not comparable(base_cell, cand_cell):
            if workload.headline:
                incomplete_headlines.append(workload.name)
            continue
        pairs.append((workload, base_cell, cand_cell))
    rows, unpaired = gate_rows(pairs, floor, candidate.thresholds.cv_bound)
    return GateResult(
        kind=GateKind.PAIRED,
        lane=candidate.run.lane,
        baseline_run=candidate.run.id,
        candidate_run=candidate.run.id,
        baseline_label=image_label(
            candidate.run.baseline_git_sha,
            candidate.run.baseline_ref,
            candidate.run.baseline_kernel_build,
            candidate.run.kernel_build,
        ),
        candidate_label=image_label(
            candidate.run.helios_git_sha,
            None,
            candidate.run.kernel_build,
            candidate.run.baseline_kernel_build,
        ),
        baseline_host=candidate.hardware.cpu,
        candidate_host=candidate.hardware.cpu,
        noise_floor=floor,
        rows=rows,
        incomplete_headlines=incomplete_headlines,
        unpaired_metrics=unpaired,
        blocking=bool(incomplete_headlines) or any(row.regression and row.headline for row in rows),
        enforced=True,
    )


def gate_report(candidate: Report, baseline: Report | None) -> GateReport:
    return GateReport(
        paired=evaluate_paired(candidate),
        cross_run=evaluate(baseline, candidate) if baseline is not None else None,
    )
