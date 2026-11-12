"""Typed pins for a benchmark run and the host checks against them."""

from __future__ import annotations

import os
import platform
import re
import shutil
import subprocess
import tomllib
from pathlib import Path
from typing import Literal

import yaml
from pydantic import BaseModel, Field, field_validator, model_validator

from helios_bench import REPO_ROOT, TOOLS_BENCH_ROOT, WASI_APPS_ROOT
from helios_bench.wasi_apps import fedora_baseline

MANIFEST_PATH = TOOLS_BENCH_ROOT / "manifest.toml"
WORKLOADS_PATH = WASI_APPS_ROOT / "workloads.json"
# Where a commit pins its Wasmtime fork, relative to the checkout.
WASMTIME_ACTION = Path(".github") / "actions" / "checkout-wasmtime" / "action.yml"
WASMTIME_ACTION_PATH = REPO_ROOT / WASMTIME_ACTION
MANIFEST_SCHEMA_VERSION = 1


class Statistics(BaseModel):
    iterations: int = Field(ge=2)
    warmup_discard: int = Field(ge=1)
    cv_bound: float = Field(gt=0)
    bootstrap_resamples: int = Field(ge=1000)
    confidence: float = Field(gt=0, lt=1)
    bootstrap_seed: int


def validate_rounds(rounds: int) -> int:
    """A run of alternating rounds is an even number of them (#491).

    Consecutive rounds swap which side goes first, so only an even count
    gives each side the first slot as often as the second.
    """
    if rounds < 2 or rounds % 2 != 0:
        raise ValueError(
            f"rounds is {rounds}: alternating the order of the two sides needs an even number of "
            "rounds, at least 2, so each side goes first as often as it goes second"
        )
    return rounds


def validate_guest_memory(lane: Lane) -> Lane:
    """The two guests of a lane get one memory size, unless the lane says why not (#491).

    A guest with more memory than the other is a different machine, so a
    lane whose fields differ without a stated reason is refused, and so is
    a reason left behind on a lane whose fields agree.
    """
    if lane.memory != lane.linux_vm_memory and lane.memory_differs_because is None:
        raise ValueError(
            f"lane {lane.name!r} gives the Helios guest memory = {lane.memory!r} and the Linux "
            f"guest linux_vm_memory = {lane.linux_vm_memory!r}: the two columns compare alike "
            "only on equal guests, so set them equal or state memory_differs_because"
        )
    if lane.memory == lane.linux_vm_memory and lane.memory_differs_because is not None:
        raise ValueError(
            f"lane {lane.name!r} states memory_differs_because, and memory and linux_vm_memory "
            f"are both {lane.memory!r}"
        )
    return lane


class SidedFact(BaseModel):
    """One fact stated for each of the two columns."""

    helios: str
    linux: str


class StatedAsymmetries(BaseModel):
    """What still differs between a lane's two columns by construction (#491).

    Facts fixed by the lane's own construction, stated once here so the
    report's pins repeat them from the manifest instead of from prose
    that could go stale. What a run can read (the Wasmtime revisions) is
    not stated here: `helios_bench.asymmetries` reads it.
    """

    stack_limit_interruption: SidedFact = Field(
        description="whether stack-limit interruption is compiled into each side's code"
    )


class Lane(BaseModel):
    name: str
    helios_arch: str
    guest_arch: str
    accelerator: str
    rpc_transport: Literal["serial", "vsock"]
    runner_label: str
    # The hosted runner the lane's job takes in advisory mode. A lane no
    # hosted runner can carry has none: it is run by hand on its dedicated
    # host and is not part of the workflow matrix.
    shared_runner: str | None = None
    qemu_version: str
    boot_timeout_seconds: int = Field(ge=1)
    vcpus: int = Field(ge=1)
    # The worker count the kernel's compiler plugin runs with on this lane's
    # guest, which the Linux + Wasmtime column of `aot-curl` is given.
    compiler_workers: int = Field(ge=1)
    memory: str
    linux_vm_memory: str
    # Why the lane gives its two guests different memory sizes. The two
    # columns compare alike only on equal guests, so a lane that sets
    # `memory` and `linux_vm_memory` apart says why, and one that keeps
    # them equal has nothing to say (#491).
    memory_differs_because: str | None = None
    net_backend: str
    net_host: str
    devices: list[str]
    # How many alternating rounds one run of the lane is: a round times both
    # sides once, and consecutive rounds swap which goes first, so host
    # drift between the two halves of a round lands on each column in turn.
    # None is a single pass, Helios then Linux.
    rounds: int | None = None
    # What still differs between the lane's two columns by construction,
    # for the lanes whose parity table states it beside the pins.
    stated_asymmetries: StatedAsymmetries | None = None

    @field_validator("rounds")
    @classmethod
    def rounds_are_even(cls, rounds: int | None) -> int | None:
        return None if rounds is None else validate_rounds(rounds)

    @model_validator(mode="after")
    def guests_have_equal_memory(self) -> Lane:
        return validate_guest_memory(self)

    @property
    def qemu_binary(self) -> str:
        return f"qemu-system-{self.guest_arch}"

    def runs_on(self, advisory: bool) -> str:
        """The runner this lane runs on in the given mode.

        An advisory run of a lane with no hosted runner is still taken on
        its dedicated host: there is nowhere else it can run.
        """
        if advisory and self.shared_runner is not None:
            return self.shared_runner
        return self.runner_label

    def github_matrix_entry(self) -> dict[str, str]:
        """One `include` entry of the bench-suite workflow matrix."""
        if self.shared_runner is None:
            raise SystemExit(
                f"lane {self.name!r} has no hosted runner and is not part of the workflow matrix; "
                f"run it on its dedicated host with `helios-bench run --lane {self.name}`"
            )
        return {
            "lane": self.name,
            "shared-runner": self.shared_runner,
            "dedicated-runner": self.runner_label,
            "helios-arch": self.helios_arch,
            "guest-arch": self.guest_arch,
            "net-backend": self.net_backend,
            # The inspector RPC transport the lane's numbers are taken
            # over; a job that boots the lane's workloads outside the
            # matrix reads it here for the same reason it reads `memory`.
            "rpc-transport": self.rpc_transport,
            "boot-timeout": str(self.boot_timeout_seconds),
            # The guest RAM the lane's workloads are sized for. A job
            # that boots this lane's workloads outside the matrix — the
            # PGO collection, the PGO comparison — reads it here rather
            # than naming a number of its own, because a workload that
            # fits in the suite and not in the collection profiles
            # nothing.
            "memory": self.memory,
        }


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


class WasmtimePin(BaseModel, frozen=True):
    """The Wasmtime fork revision a Helios commit builds against."""

    repository: str = Field(description="the GitHub `owner/name` the fork is checked out from")
    revision: str


def wasmtime_pin(action_text: str, source: str) -> WasmtimePin:
    """The pin `.github/actions/checkout-wasmtime/action.yml` states.

    `source` names where the text came from, for the refusal.
    """
    action = yaml.safe_load(action_text)
    for step in action["runs"]["steps"]:
        with_ = step.get("with", {})
        if with_.get("repository", "").endswith("/wasmtime") and "ref" in with_:
            return WasmtimePin(repository=str(with_["repository"]), revision=str(with_["ref"]))
    raise SystemExit(f"{source}: no checkout step pins a wasmtime ref")


def vendored_wasmtime_pin(path: Path = WASMTIME_ACTION_PATH) -> WasmtimePin:
    """The Wasmtime tree this checkout builds against, from its one pin."""
    return wasmtime_pin(path.read_text(encoding="utf-8"), str(path))


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
    if lane.rpc_transport == "vsock" and not os.access("/dev/vhost-vsock", os.R_OK | os.W_OK):
        deviations.append("/dev/vhost-vsock is not usable, lane carries the inspector RPC over vsock")
    cpus = platform.os.cpu_count() or 0
    if cpus < lane.vcpus:
        deviations.append(f"host has {cpus} logical CPUs, lane needs {lane.vcpus} vCPUs")
    return deviations
