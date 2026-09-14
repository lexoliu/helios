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

import re
from collections.abc import Iterable
from dataclasses import dataclass
from enum import StrEnum

from helios_bench.report import (
    Cell,
    NoiseRetry,
    Report,
    SeriesStats,
    Side,
    WorkloadResult,
)
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


#: The page every Helios target maps in. A footprint that moved by less
#: than one of them did not move: memory is handed out in pages, and
#: `memory_per_instance_bytes` is a delta of available bytes over an
#: instance count, so its last digits are accounting rather than
#: measurement.
PAGE_BYTES = 4096

#: How many samples have to lie beyond a percentile before the gate will
#: hold a change to it.
#:
#: A workload's latency metrics are computed over the samples of one
#: iteration, so `first_output_p99_us` on a hundred-way concurrent spawn
#: is computed over a hundred samples, and `LatencySamples::percentile`'s
#: nearest rank makes it the second largest of them. That is an extremum
#: wearing a percentile's name: what it records is the scheduling order of
#: the boot that produced it, and a paired run of two identical kernels
#: moved it 14% (run 34223160269, #286). Ten samples past the rank is the
#: line: p99 needs a thousand samples, p99.9 needs ten thousand, and a
#: median needs twenty.
MIN_SAMPLES_BEYOND_PERCENTILE = 10

#: Suffix of the metric each `LatencySamples::report` prints its sample
#: count under. It is context for the percentiles beside it rather than a
#: measurement of anything, so it is never a row.
SAMPLE_COUNT_SUFFIX = "_samples"

PERCENTILE = re.compile(r"_p(?P<digits>\d{2,})$")

#: The share of a column's kernel functions its profile may leave
#: uncovered before the column stops being the profiled image a paired
#: comparison is between. A collection from the commit it covers leaves
#: about 1.7–1.8% uncovered — 478 of 27,615 on run 34737497450, 501 of
#: about 27,500 on the week's pairs — and a candidate built against a
#: profile collected from another commit left 12–15%; the bound sits
#: between the two classes (#384).
UNDERPROFILED_SHARE = 0.05


def statistic_of(name: str) -> str:
    """The metric name with its unit suffix removed."""
    for suffix, _ in METRIC_UNITS:
        if name.endswith(suffix):
            return name[: -len(suffix)]
    return name


def samples_needed(name: str) -> float | None:
    """Samples a percentile needs before the gate may block on it.

    `None` for a measurement that is not a percentile of a sample. A
    maximum returns infinity: no sample size makes an extremum
    attributable to a change.
    """
    statistic = statistic_of(name)
    if statistic.endswith("_max") or statistic.endswith("_min"):
        return float("inf")
    match = PERCENTILE.search(statistic)
    if match is None:
        return None
    digits = match.group("digits")
    # `_p99` is 99%, `_p999` is 99.9%: the digits are the percentage with
    # the decimal point after the first two.
    percent = float(digits[:2] + "." + digits[2:]) if len(digits) > 2 else float(digits)
    beyond = (100.0 - percent) / 100.0
    if beyond <= 0.0:
        return float("inf")
    return MIN_SAMPLES_BEYOND_PERCENTILE / beyond


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
    #: A measurement the gate reports and never blocks on, because a shift
    #: in it is not attributable to the change: the tail of one
    #: iteration's samples belongs to that boot's scheduling order.
    diagnostic: bool = False
    diagnostic_reason: str | None = None


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
class ControlDrift:
    """The control side that set the floor: how far its median moved between
    the boot before the suite and the boot after, and the larger of the two
    series' coefficients of variation. One of the two is the floor."""

    side: Side
    drift: float
    cv: float


def worst_control(*reports: Report) -> ControlDrift | None:
    worst: tuple[float, ControlDrift] | None = None
    for report in reports:
        if report.control is None:
            continue
        for side, control_side in report.control.sides.items():
            if worst is not None and control_side.noise_floor <= worst[0]:
                continue
            drift = ControlDrift(
                side=side,
                drift=relative_shift(control_side.before.median, control_side.after.median),
                cv=max(control_side.before.cv, control_side.after.cv),
            )
            worst = (control_side.noise_floor, drift)
    return None if worst is None else worst[1]


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
    floor_bound: float
    control: ControlDrift | None
    retaken: list[str]
    reconfirmed: list[str]
    #: The candidate run's two passes' floors, when its paired suite was
    #: measured twice in one job: `noise_floor` is then the second pass's.
    noise_retry: NoiseRetry | None
    rows: list[GateRow]
    incomplete_headlines: list[str]
    unpaired_metrics: list[UnpairedMetric]
    #: The `(uncovered, functions)` each column's profile-use build
    #: recorded beside its kernel, keyed by the column's name — what
    #: `underprofiled` was computed from and what its reason names. A
    #: column whose build kept no list — the plain control above all —
    #: has no entry.
    pgo_uncovered: dict[str, tuple[int, int]]
    #: The columns whose kernel profile covered nothing about more than
    #: `UNDERPROFILED_SHARE` of the image's functions. A column past the
    #: bound was built against a profile that does not describe its
    #: commit, so it is not the profiled image the pairing is between and
    #: the comparison is inconclusive (#384).
    underprofiled: list[str]
    blocking: bool
    enforced: bool

    @property
    def inconclusive(self) -> bool:
        """The run cannot resolve the comparison.

        The floor is what the control workload says the machine moved by
        during the run. Past the bound the gate holds a single row's
        dispersion to, that movement hides any effect a change could have;
        and a column whose profile left more than `UNDERPROFILED_SHARE` of
        the kernel uncovered is not the profiled image the pairing is
        between. Either way no row gets a verdict: the run is rerun, not
        read.
        """
        return self.noise_floor > self.floor_bound or bool(self.underprofiled)

    @property
    def regressions(self) -> list[GateRow]:
        if self.inconclusive:
            return []
        return [row for row in self.rows if row.regression]

    @property
    def rejected_rows(self) -> list[GateRow]:
        return [row for row in self.rows if row.rejected]

    @property
    def diagnostic_rows(self) -> list[GateRow]:
        return [row for row in self.rows if row.diagnostic]

    @property
    def improvements(self) -> list[GateRow]:
        if self.inconclusive:
            return []
        return [row for row in self.rows if row.improvement]

    @property
    def headline_regressions(self) -> list[GateRow]:
        return [row for row in self.regressions if row.headline]


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
    samples: float | None = None,
) -> GateRow:
    """One row: two warm series of one measurement, judged the same way.

    Significant means the two bootstrap intervals of the medians are
    disjoint **and** the median moved by more than the smallest shift
    that could mean anything. Which sign of movement is the bad one, what
    that smallest shift is, and whether the row may block at all, all
    come from what the measurement is.
    """
    unit = DURATION if measurement == ELAPSED else metric_unit(measurement)
    needed = None if measurement == ELAPSED else samples_needed(measurement)
    diagnostic = needed is not None and (samples is None or samples < needed)
    shift = relative_shift(before.median, after.median)
    disjoint = not intervals_overlap(before, after)
    if unit.drifts_with_the_host:
        # The control's drift bounds how far the machine moved, which is
        # what a duration or a rate of durations is exposed to.
        beyond = abs(shift) > floor
    else:
        # A footprint is exposed to the page, not to the clock.
        beyond = abs(after.median - before.median) > PAGE_BYTES
    worse = shift > 0 if unit.direction is Direction.LOWER_IS_BETTER else shift < 0
    dispersed = max(before.cv, after.cv)
    rejected = dispersed > cv_bound
    significant = disjoint and beyond and not rejected and not diagnostic
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
        diagnostic=diagnostic,
        diagnostic_reason=diagnostic_reason(measurement, samples, needed) if diagnostic else None,
        rejection_reason=(
            f"warm coefficient of variation {dispersed:.3f} exceeds the run's bound {cv_bound:.3f}"
            if rejected
            else None
        ),
    )


def diagnostic_reason(measurement: str, samples: float | None, needed: float | None) -> str:
    """Why a row is reported rather than blocked on."""
    if needed == float("inf"):
        return "an extremum of one iteration's samples, which no sample size makes attributable"
    if samples is None:
        return (
            "a percentile whose sample count the workload does not report, so the gate cannot "
            "tell it from an extremum"
        )
    return (
        f"a percentile over {samples:,.0f} samples, and {needed:,.0f} are needed before "
        f"{MIN_SAMPLES_BEYOND_PERCENTILE} of them lie past its rank"
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
            if metric.endswith(SAMPLE_COUNT_SUFFIX):
                # Context for the percentiles beside it, not a row.
                continue
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
            rows.append(
                compare_series(
                    workload,
                    metric,
                    before,
                    after,
                    floor,
                    cv_bound,
                    samples=sample_count(base_cell, cand_cell, metric),
                )
            )
    return rows, unpaired


def sample_count(base_cell: Cell, cand_cell: Cell, metric: str) -> float | None:
    """The samples a percentile was computed over, as both cells report it.

    The smaller of the two, because a percentile is only as attributable
    as the thinner of the samples it is compared across. `None` when
    either side does not report a count — a report written before its
    harness did — which the gate reads as unknown rather than as enough.

    The count belongs to the whole family: `first_output_p99_us` and
    `first_output_max_us` are statistics of the samples counted by
    `first_output_samples`, so the name to look up is the metric's with
    its unit and its statistic taken off.
    """
    statistic = statistic_of(metric)
    if "_" not in statistic:
        return None
    counted = f"{statistic.rsplit('_', 1)[0]}{SAMPLE_COUNT_SUFFIX}"
    counts = [cell.metrics[counted].median for cell in (base_cell, cand_cell) if counted in cell.metrics]
    return min(counts) if len(counts) == 2 else None


def comparable(base_cell: Cell | None, cand_cell: Cell | None) -> bool:
    return (
        base_cell is not None and cand_cell is not None and not base_cell.rejected and not cand_cell.rejected
    )


def noise_floor(*reports: Report) -> float:
    return max((report.control.noise_floor if report.control else 0.0) for report in reports)


def kernel_profile_coverage(
    columns: Iterable[tuple[Column, int | None, int | None]],
) -> dict[str, tuple[int, int]]:
    """The (uncovered, functions) counts each column's build recorded.

    Only columns whose build kept a list appear: a plain `release` control
    reads no profile and records none, and a zero denominator is no
    denominator.
    """
    return {
        column.value: (uncovered, functions)
        for column, uncovered, functions in columns
        if uncovered is not None and functions
    }


def blocks(
    enforced: bool,
    floor: float,
    bound: float,
    incomplete: list[str],
    rows: list[GateRow],
    underprofiled: list[str],
) -> bool:
    """Whether an enforced comparison fails the check.

    A floor past the bound blocks before any row is read: the host was too
    noisy to measure the change, and a green check on such a run would let
    a real regression through as noise. An under-profiled column blocks
    the same way: what ran is not the comparison the table answers.
    """
    if not enforced:
        return False
    if floor > bound or underprofiled:
        return True
    return bool(incomplete) or any(row.regression and row.headline for row in rows)


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
    bound = candidate.thresholds.cv_bound
    rows, unpaired = gate_rows(pairs, floor, bound)
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
        floor_bound=bound,
        control=worst_control(baseline, candidate),
        retaken=list(candidate.run.retaken),
        reconfirmed=list(candidate.run.reconfirmed),
        noise_retry=candidate.run.noise_retry,
        rows=rows,
        incomplete_headlines=[],
        unpaired_metrics=unpaired,
        # The under-profiled refusal is the paired instrument's: it is the
        # run that was asked to collect a profile per column (#384).
        pgo_uncovered={},
        underprofiled=[],
        blocking=blocks(enforced, floor, bound, [], rows, []),
        enforced=enforced,
    )


def image_label(
    sha: str | None,
    ref: str | None,
    build: str | None,
    other_build: str | None,
    profile: str | None = None,
    other_profile: str | None = None,
    uncovered: int | None = None,
    functions: int | None = None,
) -> str:
    """How one column of a paired table names its image.

    The commit always, then whatever distinguishes this image from the
    other one: the ref it was asked for, the cargo profile its kernel was
    built with when the two differ, and the kernel profile it was built
    against when those differ — a PGO pairing varies the build and not
    the commit, and once every release build reads a profile it varies
    which profile (#226), so without these the two columns would carry
    the same label. A profile-use column also names how many of the
    kernel's functions its profile covered nothing about (#329).
    """
    qualifiers = []
    if ref:
        qualifiers.append(ref)
    if build and other_build and build != other_build:
        qualifiers.append(build)
    if profile and profile != other_profile:
        qualifiers.append(profile)
    if uncovered is not None and functions is not None:
        qualifiers.append(f"{uncovered:,} of {functions:,} functions uncovered")
    elif uncovered is not None:
        qualifiers.append(f"{uncovered:,} uncovered functions")
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
    bound = candidate.thresholds.cv_bound
    rows, unpaired = gate_rows(pairs, floor, bound)
    # What each column's profile covered of its own kernel decides whether
    # the comparison is the one it claims: a column built against a
    # profile collected from another commit is not the profiled image the
    # pairing is between, and the verdict cannot separate the change's
    # effect from the stale profile's (#384). The rule is the commit
    # pairing's — two commits, each owed a profile of its own. A pairing
    # of one commit against itself varies the profile on purpose: the
    # fetched profile against this run's collection (`suite-pgo`), where
    # an under-profiled column is the measurement, not a fault in it.
    pgo_uncovered = kernel_profile_coverage(
        [
            (
                Column.BASELINE,
                candidate.run.baseline_kernel_pgo_uncovered,
                candidate.run.baseline_kernel_pgo_functions,
            ),
            (
                Column.CANDIDATE,
                candidate.run.kernel_pgo_uncovered,
                candidate.run.kernel_pgo_functions,
            ),
        ]
    )
    underprofiled = [
        name
        for name, (uncovered, functions) in pgo_uncovered.items()
        if candidate.run.baseline_ref is not None and uncovered / functions > UNDERPROFILED_SHARE
    ]
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
            candidate.run.baseline_kernel_profile,
            candidate.run.kernel_profile,
            candidate.run.baseline_kernel_pgo_uncovered,
            candidate.run.baseline_kernel_pgo_functions,
        ),
        candidate_label=image_label(
            candidate.run.helios_git_sha,
            None,
            candidate.run.kernel_build,
            candidate.run.baseline_kernel_build,
            candidate.run.kernel_profile,
            candidate.run.baseline_kernel_profile,
            candidate.run.kernel_pgo_uncovered,
            candidate.run.kernel_pgo_functions,
        ),
        baseline_host=candidate.hardware.cpu,
        candidate_host=candidate.hardware.cpu,
        noise_floor=floor,
        floor_bound=bound,
        control=worst_control(candidate),
        retaken=list(candidate.run.retaken),
        reconfirmed=list(candidate.run.reconfirmed),
        noise_retry=candidate.run.noise_retry,
        rows=rows,
        incomplete_headlines=incomplete_headlines,
        unpaired_metrics=unpaired,
        pgo_uncovered=pgo_uncovered,
        underprofiled=underprofiled,
        blocking=blocks(True, floor, bound, incomplete_headlines, rows, underprofiled),
        enforced=True,
    )


def gate_report(candidate: Report, baseline: Report | None) -> GateReport:
    return GateReport(
        paired=evaluate_paired(candidate),
        cross_run=evaluate(baseline, candidate) if baseline is not None else None,
    )
