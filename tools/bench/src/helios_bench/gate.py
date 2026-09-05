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
"""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass
from enum import StrEnum

from helios_bench.report import Cell, Report, Side, WorkloadResult
from helios_bench.stats import intervals_overlap, relative_shift


class GateKind(StrEnum):
    PAIRED = "paired"
    CROSS_RUN = "cross_run"


GATE_TITLES = {
    GateKind.PAIRED: "Paired, one host, one job",
    GateKind.CROSS_RUN: "Cross-run, against the latest `dev` report",
}


@dataclass(frozen=True)
class GateRow:
    workload: str
    headline: bool
    baseline_median: float
    candidate_median: float
    shift: float
    ci_disjoint: bool
    beyond_noise: bool
    regression: bool
    improvement: bool


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
    blocking: bool
    enforced: bool

    @property
    def regressions(self) -> list[GateRow]:
        return [row for row in self.rows if row.regression]

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


def gate_rows(pairs: Iterable[tuple[WorkloadResult, Cell, Cell]], floor: float) -> list[GateRow]:
    """A regression is significant when the two warm bootstrap intervals of
    the compared cells do not overlap and the median moved by more than the
    noise floor the two runs measured."""
    rows = []
    for workload, base_cell, cand_cell in pairs:
        shift = relative_shift(base_cell.warm.median, cand_cell.warm.median)
        disjoint = not intervals_overlap(base_cell.warm, cand_cell.warm)
        beyond = abs(shift) > floor
        rows.append(
            GateRow(
                workload=workload.name,
                headline=workload.headline,
                baseline_median=base_cell.warm.median,
                candidate_median=cand_cell.warm.median,
                shift=shift,
                ci_disjoint=disjoint,
                beyond_noise=beyond,
                regression=disjoint and beyond and shift > 0,
                improvement=disjoint and beyond and shift < 0,
            )
        )
    return rows


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
    rows = gate_rows(pairs, floor)
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
        blocking=enforced and any(row.regression and row.headline for row in rows),
        enforced=enforced,
    )


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
    for workload in candidate.workloads:
        base_cell = workload.cells.get(Side.HELIOS_BASELINE)
        cand_cell = workload.cells.get(Side.HELIOS)
        if not comparable(base_cell, cand_cell):
            continue
        pairs.append((workload, base_cell, cand_cell))
    rows = gate_rows(pairs, floor)
    return GateResult(
        kind=GateKind.PAIRED,
        lane=candidate.run.lane,
        baseline_run=candidate.run.id,
        candidate_run=candidate.run.id,
        baseline_label=f"`{short(candidate.run.baseline_git_sha)}`"
        + (f" ({candidate.run.baseline_ref})" if candidate.run.baseline_ref else ""),
        candidate_label=f"`{short(candidate.run.helios_git_sha)}`",
        baseline_host=candidate.hardware.cpu,
        candidate_host=candidate.hardware.cpu,
        noise_floor=floor,
        rows=rows,
        blocking=any(row.regression and row.headline for row in rows),
        enforced=True,
    )


def gate_report(candidate: Report, baseline: Report | None) -> GateReport:
    return GateReport(
        paired=evaluate_paired(candidate),
        cross_run=evaluate(baseline, candidate) if baseline is not None else None,
    )
