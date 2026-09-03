"""Regression gate: a candidate report against a baseline from the same lane."""

from __future__ import annotations

from dataclasses import dataclass

from helios_bench.report import Report, Side
from helios_bench.stats import intervals_overlap, relative_shift


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
    lane: str
    baseline_run: str | None
    candidate_run: str | None
    noise_floor: float
    rows: list[GateRow]
    blocking: bool
    enforced: bool

    @property
    def regressions(self) -> list[GateRow]:
        return [row for row in self.rows if row.regression]

    @property
    def headline_regressions(self) -> list[GateRow]:
        return [row for row in self.rows if row.regression and row.headline]


def evaluate(baseline: Report, candidate: Report) -> GateResult:
    """A regression is significant when the two warm bootstrap intervals of
    the Helios cell do not overlap and the median moved by more than the
    larger of the two runs' noise floors."""
    if baseline.run.lane != candidate.run.lane:
        raise SystemExit(
            "gate compares reports from one lane; "
            f"baseline is {baseline.run.lane}, candidate is {candidate.run.lane}"
        )
    floor = max(
        baseline.control.noise_floor if baseline.control else 0.0,
        candidate.control.noise_floor if candidate.control else 0.0,
    )
    rows = []
    for workload in candidate.workloads:
        before = baseline.workload(workload.name)
        if before is None:
            continue
        base_cell = before.cells.get(Side.HELIOS)
        cand_cell = workload.cells.get(Side.HELIOS)
        if base_cell is None or cand_cell is None or base_cell.rejected or cand_cell.rejected:
            continue
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
    enforced = baseline.run.publishable and candidate.run.publishable
    blocking = enforced and any(row.regression and row.headline for row in rows)
    return GateResult(
        lane=candidate.run.lane,
        baseline_run=baseline.run.id,
        candidate_run=candidate.run.id,
        noise_floor=floor,
        rows=rows,
        blocking=blocking,
        enforced=enforced,
    )
