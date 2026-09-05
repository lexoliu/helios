"""SVG plots of a report with matplotlib."""

from __future__ import annotations

from pathlib import Path

import matplotlib

matplotlib.use("Agg")

import matplotlib.pyplot as plt  # noqa: E402

from helios_bench.report import CLASS_LABELS, SIDE_LABELS, Report, Side, WorkloadClass  # noqa: E402

SIDE_COLORS = {
    Side.HELIOS: "#d97706",
    Side.HELIOS_BASELINE: "#b45309",
    Side.LINUX_WASMTIME: "#2563eb",
    Side.LINUX_NATIVE: "#6b7280",
}


def plot_class(report: Report, workload_class: WorkloadClass, path: Path) -> None:
    """Median warm wall time per side with the bootstrap interval as error
    bars, one row per workload, log scale so start-up and compute fit."""
    workloads = [workload for workload in report.workloads if workload.workload_class == workload_class]
    if not workloads:
        raise ValueError(f"report has no {workload_class} workloads")
    sides = report.table_sides()
    height = 0.8 / len(sides)
    figure, axis = plt.subplots(figsize=(9, 0.9 + 0.55 * len(workloads)))
    for offset, side in enumerate(sides):
        positions = []
        medians = []
        lows = []
        highs = []
        for row, workload in enumerate(workloads):
            cell = workload.cells.get(side)
            if cell is None:
                continue
            positions.append(row + (offset - (len(sides) - 1) / 2) * height)
            medians.append(cell.warm.median)
            lows.append(cell.warm.median - cell.warm.ci_low)
            highs.append(cell.warm.ci_high - cell.warm.median)
        if not positions:
            continue
        axis.barh(
            positions,
            medians,
            height=height,
            xerr=[lows, highs],
            color=SIDE_COLORS[side],
            label=SIDE_LABELS[side],
            capsize=3,
        )
    axis.set_yticks(range(len(workloads)))
    axis.set_yticklabels([workload.name for workload in workloads])
    axis.invert_yaxis()
    axis.set_xscale("log")
    axis.set_xlabel("median warm wall time, ms (log scale; bars are 95% bootstrap CI)")
    axis.set_title(f"{CLASS_LABELS[workload_class]} — {report.run.lane}")
    axis.legend(loc="lower right")
    axis.grid(axis="x", which="both", alpha=0.3)
    figure.tight_layout()
    path.parent.mkdir(parents=True, exist_ok=True)
    figure.savefig(path, format="svg")
    plt.close(figure)


def plot_headline(report: Report, path: Path) -> None:
    """Speed-up of Helios over each Linux side for the headline workloads;
    values below 1 are where Helios is slower."""
    workloads = report.headline_workloads()
    if not workloads:
        raise ValueError("report has no headline workloads")
    sides = [Side.LINUX_WASMTIME, Side.LINUX_NATIVE]
    height = 0.8 / len(sides)
    figure, axis = plt.subplots(figsize=(9, 0.9 + 0.5 * len(workloads)))
    for offset, side in enumerate(sides):
        positions = []
        values = []
        for row, workload in enumerate(workloads):
            for comparison in workload.comparisons:
                if comparison.against == side:
                    positions.append(row + (offset - 0.5) * height)
                    values.append(comparison.speedup)
        if positions:
            axis.barh(
                positions, values, height=height, color=SIDE_COLORS[side], label=f"vs {SIDE_LABELS[side]}"
            )
    axis.axvline(1.0, color="black", linewidth=1)
    axis.set_yticks(range(len(workloads)))
    axis.set_yticklabels([workload.name for workload in workloads])
    axis.invert_yaxis()
    axis.set_xscale("log")
    axis.set_xlabel("Helios speed-up (other median / Helios median, log scale)")
    axis.set_title(f"Headline workloads — {report.run.lane}")
    axis.legend(loc="lower right")
    axis.grid(axis="x", which="both", alpha=0.3)
    figure.tight_layout()
    path.parent.mkdir(parents=True, exist_ok=True)
    figure.savefig(path, format="svg")
    plt.close(figure)


def plot_report(report: Report, out_dir: Path) -> list[Path]:
    written = []
    for workload_class in report.by_class():
        path = out_dir / f"{report.run.lane}-{workload_class}.svg"
        plot_class(report, workload_class, path)
        written.append(path)
    if report.headline_workloads():
        path = out_dir / f"{report.run.lane}-headline.svg"
        plot_headline(report, path)
        written.append(path)
    return written
