"""Typed pins for a benchmark run and the host checks against them."""

from __future__ import annotations

import platform
import re
import shutil
import subprocess
import tomllib
from pathlib import Path

import yaml
from pydantic import BaseModel, Field

from helios_bench import REPO_ROOT, TOOLS_BENCH_ROOT, WASI_APPS_ROOT
from helios_bench.wasi_apps import fedora_baseline

MANIFEST_PATH = TOOLS_BENCH_ROOT / "manifest.toml"
WORKLOADS_PATH = WASI_APPS_ROOT / "workloads.json"
WASMTIME_ACTION_PATH = REPO_ROOT / ".github" / "actions" / "checkout-wasmtime" / "action.yml"
MANIFEST_SCHEMA_VERSION = 1


class Statistics(BaseModel):
    iterations: int = Field(ge=2)
    warmup_discard: int = Field(ge=1)
    cv_bound: float = Field(gt=0)
    bootstrap_resamples: int = Field(ge=1000)
    confidence: float = Field(gt=0, lt=1)
    bootstrap_seed: int


class Lane(BaseModel):
    name: str
    helios_arch: str
    guest_arch: str
    accelerator: str
    runner_label: str
    shared_runner: str
    qemu_version: str
    vcpus: int = Field(ge=1)
    memory: str
    linux_vm_memory: str
    net_backend: str
    net_host: str
    devices: list[str]

    @property
    def qemu_binary(self) -> str:
        return f"qemu-system-{self.guest_arch}"


class Manifest(BaseModel):
    schema_version: int
    statistics: Statistics
    lanes: list[Lane] = Field(alias="lane")

    model_config = {"populate_by_name": True}

    def lane(self, name: str) -> Lane:
        for lane in self.lanes:
            if lane.name == name:
                return lane
        raise SystemExit(f"manifest has no lane {name!r}; known lanes: {[lane.name for lane in self.lanes]}")


def load_manifest(path: Path = MANIFEST_PATH) -> Manifest:
    with path.open("rb") as handle:
        raw = tomllib.load(handle)
    if raw.get("schema_version") != MANIFEST_SCHEMA_VERSION:
        raise SystemExit(
            f"{path}: unsupported schema_version {raw.get('schema_version')}, "
            f"expected {MANIFEST_SCHEMA_VERSION}"
        )
    return Manifest.model_validate(raw)


def vendored_wasmtime_revision(path: Path = WASMTIME_ACTION_PATH) -> str:
    """The Wasmtime tree every workflow builds against, from its one pin."""
    with path.open("r", encoding="utf-8") as handle:
        action = yaml.safe_load(handle)
    for step in action["runs"]["steps"]:
        with_ = step.get("with", {})
        if with_.get("repository", "").endswith("/wasmtime") and "ref" in with_:
            return str(with_["ref"])
    raise SystemExit(f"{path}: no checkout step pins a wasmtime ref")


def fedora_image(guest_arch: str) -> tuple[str, str]:
    """URL and SHA256 of the pinned Fedora cloud image for ``guest_arch``."""
    images = fedora_baseline().FEDORA_IMAGES
    if guest_arch not in images:
        raise SystemExit(f"no pinned Fedora image for guest arch {guest_arch}")
    return images[guest_arch]["url"], images[guest_arch]["sha256"]


def wasmtime_linux_release(guest_arch: str) -> str:
    return fedora_baseline().wasmtime_linux_release(guest_arch)[0]


def qemu_version(binary: str) -> str | None:
    """The version QEMU reports, or ``None`` when the binary is absent."""
    if shutil.which(binary) is None:
        return None
    output = subprocess.run([binary, "--version"], capture_output=True, text=True, check=True).stdout
    match = re.search(r"version (\d+\.\d+\.\d+)", output)
    if match is None:
        raise SystemExit(f"{binary} --version printed no version: {output!r}")
    return match.group(1)


def accelerator_available(accelerator: str) -> bool:
    if accelerator == "kvm":
        return Path("/dev/kvm").exists()
    if accelerator == "hvf":
        if platform.system() != "Darwin":
            return False
        output = subprocess.run(
            ["sysctl", "-n", "kern.hv_support"], capture_output=True, text=True, check=False
        )
        return output.stdout.strip() == "1"
    raise SystemExit(f"unknown accelerator {accelerator}")


def host_cpu_model() -> str:
    system = platform.system()
    if system == "Darwin":
        return subprocess.run(
            ["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True, check=True
        ).stdout.strip()
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        for line in cpuinfo.read_text(encoding="utf-8").splitlines():
            if line.lower().startswith(("model name", "cpu model")):
                return line.partition(":")[2].strip()
        return platform.processor() or platform.machine()
    return platform.processor() or platform.machine()


def host_memory_bytes() -> int:
    system = platform.system()
    if system == "Darwin":
        return int(
            subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True, check=True).stdout
        )
    for line in Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
        if line.startswith("MemTotal:"):
            return int(line.split()[1]) * 1024
    raise SystemExit("cannot determine host memory")


def host_arch() -> str:
    machine = platform.machine()
    return {"arm64": "aarch64", "AMD64": "x86_64"}.get(machine, machine)


def host_deviations(lane: Lane) -> list[str]:
    """Every way this host differs from what ``lane`` pins.

    An empty list is the only state in which a run may be published; the
    runner refuses to start on a non-empty list unless the run is advisory,
    in which case the list is recorded in the report.
    """
    deviations = []
    if host_arch() != lane.guest_arch:
        deviations.append(f"host architecture {host_arch()} is not the lane's {lane.guest_arch}")
    version = qemu_version(lane.qemu_binary)
    if version is None:
        deviations.append(f"{lane.qemu_binary} is not installed")
    elif version != lane.qemu_version:
        deviations.append(f"{lane.qemu_binary} is {version}, lane pins {lane.qemu_version}")
    if not accelerator_available(lane.accelerator):
        deviations.append(f"accelerator {lane.accelerator} is not available on this host")
    cpus = platform.os.cpu_count() or 0
    if cpus < lane.vcpus:
        deviations.append(f"host has {cpus} logical CPUs, lane needs {lane.vcpus} vCPUs")
    return deviations
