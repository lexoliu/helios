"""The report a benchmark run produces: one JSON document, fully typed.

Everything a reader needs to reproduce or dispute a number is in here:
the hardware, every pin, every iteration of every cell, the derived
statistics, and the verdicts the tables print.
"""

from __future__ import annotations

import json
from enum import StrEnum
from pathlib import Path

from pydantic import BaseModel, Field

REPORT_SCHEMA_VERSION = 1


class Side(StrEnum):
    HELIOS = "helios"
    #: A second Helios image, built from another commit and timed against
    #: `HELIOS` on the same host in the same job. Present only in a paired
    #: run; see `helios_bench.baseline`.
    HELIOS_BASELINE = "helios_baseline"
    LINUX_WASMTIME = "linux_wasmtime"
    LINUX_NATIVE = "linux_native"


SIDE_LABELS = {
    Side.HELIOS: "Helios",
    Side.HELIOS_BASELINE: "Helios (baseline)",
    Side.LINUX_WASMTIME: "Linux + Wasmtime",
    Side.LINUX_NATIVE: "Native Linux",
}

#: The order a table and a plot put the sides in. The baseline column is
#: printed only by a run that produced one, so an unpaired report renders
#: exactly as it did before the paired mode existed.
TABLE_SIDE_ORDER = [Side.HELIOS, Side.HELIOS_BASELINE, Side.LINUX_WASMTIME, Side.LINUX_NATIVE]


class WorkloadClass(StrEnum):
    STARTUP = "startup"
    HOSTCALL = "hostcall"
    IPC = "ipc"
    SCHED = "sched"
    NET = "net"
    FS = "fs"
    COMPUTE = "compute"


CLASS_LABELS = {
    WorkloadClass.STARTUP: "Instance start-up",
    WorkloadClass.HOSTCALL: "Host call vs syscall",
    WorkloadClass.IPC: "IPC",
    WorkloadClass.SCHED: "Scheduling",
    WorkloadClass.NET: "Network",
    WorkloadClass.FS: "File I/O",
    WorkloadClass.COMPUTE: "Compute parity",
}


class RunInfo(BaseModel):
    id: str | None
    url: str | None
    attempt: int | None
    lane: str
    runner_label: str
    advisory: bool
    publishable: bool
    deviations: list[str]
    started_at: str
    finished_at: str
    helios_git_sha: str = Field(description="the candidate: the commit the `helios` side was built from")
    baseline_git_sha: str | None = Field(
        default=None,
        description="the commit the `helios_baseline` side was built from, when the run was paired",
    )
    baseline_ref: str | None = Field(
        default=None, description="what the run was asked to pair against, before it was resolved"
    )

    @property
    def paired(self) -> bool:
        return self.baseline_git_sha is not None


class Hardware(BaseModel):
    host_os: str
    host_arch: str
    cpu: str
    logical_cpus: int
    memory_bytes: int
    accelerator: str
    qemu_version: str | None


class Pins(BaseModel):
    wasmtime_revision: str
    wasmtime_linux_release: str
    fedora_image_url: str
    fedora_image_sha256: str
    qemu_version: str
    vcpus: int
    memory: str
    linux_vm_memory: str
    net_backend: str
    devices: list[str]
    wasm_artifacts: dict[str, str] = Field(
        description="repo-relative path -> sha256 of every wasm the run used"
    )
    bootfs_cwasm: dict[str, str] = Field(
        description="bootfs asset -> sha256 of the signed cwasm Helios loaded"
    )


class Thresholds(BaseModel):
    iterations: int
    warmup_discard: int
    cv_bound: float
    bootstrap_resamples: int
    confidence: float
    bootstrap_seed: int


class Iteration(BaseModel):
    index: int
    elapsed_ms: float
    cold: bool
    metrics: dict[str, float]


class SeriesStats(BaseModel):
    count: int
    median: float
    q1: float
    q3: float
    iqr: float
    mean: float
    stdev: float
    cv: float
    ci_low: float
    ci_high: float
    min: float
    max: float


class Cell(BaseModel):
    side: Side
    iterations: list[Iteration]
    cold: SeriesStats
    warm: SeriesStats
    rejected: bool
    rejection_reason: str | None
    metrics: dict[str, SeriesStats] = Field(description="warm-series statistics of every bench.<name> metric")


class Comparison(BaseModel):
    against: Side
    speedup: float = Field(
        description="other median / Helios median over the warm series; >1 means Helios is faster"
    )
    significant: bool = Field(description="the two bootstrap intervals do not overlap")
    beyond_noise: bool = Field(description="|speedup - 1| exceeds the run's noise floor")


class WorkloadResult(BaseModel):
    name: str
    workload_class: WorkloadClass
    headline: bool
    description: str
    throughput_bytes: int | None
    cells: dict[Side, Cell]
    failures: dict[Side, str] = Field(
        default_factory=dict,
        description="sides that could not measure this workload, with the harness's reason",
    )
    comparisons: list[Comparison]
    parity_bug: bool = Field(
        description="compute-class workload where Helios is significantly slower than Linux + Wasmtime"
    )


class ControlSide(BaseModel):
    before: SeriesStats
    after: SeriesStats
    noise_floor: float


class Control(BaseModel):
    workload: str
    sides: dict[Side, ControlSide]
    noise_floor: float = Field(description="largest per-side floor; what every comparison must clear")


class Report(BaseModel):
    schema_version: int = REPORT_SCHEMA_VERSION
    run: RunInfo
    hardware: Hardware
    pins: Pins
    thresholds: Thresholds
    control: Control | None
    workloads: list[WorkloadResult]

    def workload(self, name: str) -> WorkloadResult | None:
        for workload in self.workloads:
            if workload.name == name:
                return workload
        return None

    def measured_sides(self) -> set[Side]:
        return {side for workload in self.workloads for side in workload.cells}

    def table_sides(self) -> list[Side]:
        """The columns a table and a plot print, in report order."""
        measured = self.measured_sides()
        return [side for side in TABLE_SIDE_ORDER if side is not Side.HELIOS_BASELINE or side in measured]

    def headline_workloads(self) -> list[WorkloadResult]:
        return [workload for workload in self.workloads if workload.headline]

    def by_class(self) -> dict[WorkloadClass, list[WorkloadResult]]:
        grouped: dict[WorkloadClass, list[WorkloadResult]] = {}
        for workload_class in WorkloadClass:
            members = [workload for workload in self.workloads if workload.workload_class == workload_class]
            if members:
                grouped[workload_class] = members
        return grouped


def load_report(path: Path) -> Report:
    with path.open("r", encoding="utf-8") as handle:
        raw = json.load(handle)
    if raw.get("schema_version") != REPORT_SCHEMA_VERSION:
        raise SystemExit(f"{path}: unsupported report schema_version {raw.get('schema_version')}")
    return Report.model_validate(raw)


def save_report(report: Report, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(report.model_dump_json(indent=2) + "\n", encoding="utf-8")
