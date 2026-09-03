"""Drives one benchmark run on one lane and assembles its report.

The harness under tools/wasi-apps already knows how to boot Helios,
provision the pinned Fedora guest, and time every side; this module only
decides what to run, refuses hosts that deviate from the lane, records
every pin, and turns the raw JSONL into a report.
"""

from __future__ import annotations

import hashlib
import os
import platform
import shlex
import subprocess
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

from helios_bench import REPO_ROOT, WASI_APPS_ROOT
from helios_bench.assemble import assemble_report, build_control
from helios_bench.manifest import (
    Lane,
    Manifest,
    fedora_image,
    host_arch,
    host_cpu_model,
    host_deviations,
    host_memory_bytes,
    qemu_version,
    vendored_wasmtime_revision,
    wasmtime_linux_release,
)
from helios_bench.report import Hardware, Pins, Report, RunInfo, Side, Thresholds
from helios_bench.sources import RawSide, read_control, read_optional_side
from helios_bench.wasi_apps import workload_runner
from helios_bench.workloads import load_workloads, select_workloads

GAP_BENCH = WASI_APPS_ROOT / "linux-gap-bench.py"
NATIVE_BUILD = REPO_ROOT / "tools" / "bench" / "native" / "build.sh"
NATIVE_ARTIFACTS = REPO_ROOT / "artifacts" / "bench-native"
BOOT_ARTIFACTS = WASI_APPS_ROOT / "boot-artifacts.toml"
CARGO_TARGETS = {"aarch64": "aarch64-unknown-none", "x86-64": "x86_64-unknown-none"}
HELIOS_OUT = "helios"
LINUX_OUT = "linux"
LINUX_SIDES = {Side.LINUX_NATIVE, Side.LINUX_WASMTIME}


@dataclass(frozen=True)
class NetworkOptions:
    ifname: str | None = None
    bridge: str | None = None
    queues: int | None = None


@dataclass(frozen=True)
class RunOptions:
    lane: Lane
    out_dir: Path
    advisory: bool
    sides: frozenset[Side]
    workload_names: list[str] = field(default_factory=list)
    iterations: int | None = None
    runner_label: str | None = None
    allow_busy_host: bool = False
    helios_timeout_seconds: int = 9000
    linux_setup_timeout_seconds: int = 5400
    network: NetworkOptions = NetworkOptions()


@dataclass(frozen=True)
class PlannedCommand:
    description: str
    argv: list[str]
    env: dict[str, str]
    cwd: Path

    def shell(self) -> str:
        exports = " ".join(f"{key}={shlex.quote(value)}" for key, value in sorted(self.env.items()))
        return f"{exports} {shlex.join(self.argv)}".strip()


def git_sha() -> str:
    return subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=REPO_ROOT, capture_output=True, text=True, check=True
    ).stdout.strip()


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def plan(options: RunOptions, manifest: Manifest, workloads: list[dict]) -> list[PlannedCommand]:
    lane = options.lane
    iterations = options.iterations or manifest.statistics.iterations
    commands = []
    if options.sides & LINUX_SIDES:
        commands.append(
            PlannedCommand(
                description=f"build the native counterparts for {lane.guest_arch}",
                argv=[str(NATIVE_BUILD), lane.guest_arch],
                env={},
                cwd=REPO_ROOT,
            )
        )
    common = [
        "python3",
        str(GAP_BENCH),
        "--iterations",
        str(iterations),
        "--control",
        # Every cell of the report is accounted for: a workload that fails
        # is recorded as failed on that side and the run goes on.
        "--keep-going",
        "--helios-host-http-host",
        lane.net_host,
        "--helios-host-tcp-host",
        lane.net_host,
    ]
    if options.allow_busy_host:
        common.append("--allow-busy-host")
    for workload in workloads:
        common.extend(["--workload", workload["name"]])
    if Side.HELIOS in options.sides:
        env = {
            "HELIOS_WORKLOAD_BENCH_VM_MEMORY": lane.memory,
            "HELIOS_WORKLOAD_BENCH_VM_SMP": str(lane.vcpus),
            "HELIOS_WORKLOAD_BENCH_NET_BACKEND": lane.net_backend,
        }
        if options.network.ifname:
            env["HELIOS_WORKLOAD_BENCH_NET_IFNAME"] = options.network.ifname
        if options.network.bridge:
            env["HELIOS_WORKLOAD_BENCH_NET_BRIDGE"] = options.network.bridge
        if options.network.queues:
            env["HELIOS_WORKLOAD_BENCH_NET_QUEUES"] = str(options.network.queues)
        commands.append(
            PlannedCommand(
                description="time every workload on Helios",
                argv=[
                    *common,
                    "--arch",
                    lane.helios_arch,
                    "--skip-linux",
                    "--helios-timeout-seconds",
                    str(options.helios_timeout_seconds),
                    "--out-dir",
                    str(options.out_dir / HELIOS_OUT),
                ],
                env=env,
                cwd=REPO_ROOT,
            )
        )
    if options.sides & LINUX_SIDES:
        commands.append(
            PlannedCommand(
                description="time every counterpart in the pinned Fedora guest",
                argv=[
                    *common,
                    "--skip-helios",
                    "--linux-guest-arch",
                    lane.guest_arch,
                    "--linux-vm-accel",
                    lane.accelerator,
                    "--linux-vm-memory",
                    lane.linux_vm_memory,
                    "--linux-vm-smp",
                    str(lane.vcpus),
                    "--linux-vm-setup-timeout-seconds",
                    str(options.linux_setup_timeout_seconds),
                    "--native-bin-dir",
                    str(NATIVE_ARTIFACTS / lane.guest_arch),
                    "--out-dir",
                    str(options.out_dir / LINUX_OUT),
                ],
                env={},
                cwd=REPO_ROOT,
            )
        )
    return commands


def execute(command: PlannedCommand) -> None:
    env = os.environ.copy()
    env.update(command.env)
    print(f"==> {command.description}\n    {command.shell()}", flush=True)
    subprocess.run(command.argv, cwd=command.cwd, env=env, check=True)


def wasm_artifact_digests(workloads: list[dict]) -> dict[str, str]:
    """SHA256 of every wasm any side ran, keyed by repo-relative path."""
    runner = workload_runner()
    digests: dict[str, str] = {}
    for path in runner.guest_paths(REPO_ROOT, workloads):
        if path.is_file() and path.suffix == ".wasm":
            digests[str(path.relative_to(REPO_ROOT))] = sha256_of(path)
    import tomllib

    with BOOT_ARTIFACTS.open("rb") as handle:
        boot = tomllib.load(handle)
    needed = {"dash", "debugger"}
    for workload in workloads:
        needed.update(workload.get("boot_programs", []))
    for artifact in boot["artifact"]:
        if artifact["command"] in needed:
            source = REPO_ROOT / artifact["source"]
            if source.is_file():
                digests[artifact["source"]] = sha256_of(source)
    return dict(sorted(digests.items()))


def bootfs_cwasm_digests(lane: Lane) -> dict[str, str]:
    """SHA256 of the signed cwasm files the Helios guest loaded."""
    prebuild = REPO_ROOT / "target" / "kernel-prebuild" / CARGO_TARGETS[lane.helios_arch] / "release"
    if not prebuild.is_dir():
        return {}
    return {path.name: sha256_of(path) for path in sorted(prebuild.glob("*.cwasm"))}


def collect_pins(lane: Lane, workloads: list[dict]) -> Pins:
    image_url, image_sha256 = fedora_image(lane.guest_arch)
    return Pins(
        wasmtime_revision=vendored_wasmtime_revision(),
        wasmtime_linux_release=wasmtime_linux_release(lane.guest_arch),
        fedora_image_url=image_url,
        fedora_image_sha256=image_sha256,
        qemu_version=lane.qemu_version,
        vcpus=lane.vcpus,
        memory=lane.memory,
        linux_vm_memory=lane.linux_vm_memory,
        net_backend=lane.net_backend,
        devices=lane.devices,
        wasm_artifacts=wasm_artifact_digests(workloads),
        bootfs_cwasm=bootfs_cwasm_digests(lane),
    )


def collect_hardware(lane: Lane) -> Hardware:
    return Hardware(
        host_os=f"{platform.system()} {platform.release()}",
        host_arch=host_arch(),
        cpu=host_cpu_model(),
        logical_cpus=os.cpu_count() or 0,
        memory_bytes=host_memory_bytes(),
        accelerator=lane.accelerator,
        qemu_version=qemu_version(lane.qemu_binary),
    )


def github_run() -> tuple[str | None, str | None, int | None]:
    run_id = os.environ.get("GITHUB_RUN_ID")
    if not run_id:
        return None, None, None
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    attempt = os.environ.get("GITHUB_RUN_ATTEMPT")
    return run_id, f"{server}/{repository}/actions/runs/{run_id}", int(attempt) if attempt else None


def thresholds_from(manifest: Manifest, iterations: int | None) -> Thresholds:
    statistics = manifest.statistics
    return Thresholds(
        iterations=iterations or statistics.iterations,
        warmup_discard=statistics.warmup_discard,
        cv_bound=statistics.cv_bound,
        bootstrap_resamples=statistics.bootstrap_resamples,
        confidence=statistics.confidence,
        bootstrap_seed=statistics.bootstrap_seed,
    )


def read_sides(options: RunOptions, thresholds: Thresholds) -> dict[Side, RawSide]:
    sides = {}
    for side in options.sides:
        out_dir = options.out_dir / (HELIOS_OUT if side is Side.HELIOS else LINUX_OUT)
        raw = read_optional_side(out_dir, side, thresholds.warmup_discard)
        if raw is None:
            raise SystemExit(f"the {side} side produced no JSONL under {out_dir}")
        sides[side] = raw
    return sides


def read_controls(options: RunOptions, thresholds: Thresholds) -> dict[Side, tuple[RawSide, RawSide]]:
    controls = {}
    for side in options.sides:
        out_dir = options.out_dir / (HELIOS_OUT if side is Side.HELIOS else LINUX_OUT)
        before = read_control(out_dir, side, "before", thresholds.warmup_discard)
        after = read_control(out_dir, side, "after", thresholds.warmup_discard)
        if before is not None and after is not None:
            controls[side] = (before, after)
    return controls


def run_suite(options: RunOptions, manifest: Manifest, dry_run: bool = False) -> Report | None:
    lane = options.lane
    deviations = host_deviations(lane)
    if deviations and not options.advisory:
        raise SystemExit(
            "this host deviates from lane "
            f"{lane.name}; refusing to produce a publishable report:\n  - " + "\n  - ".join(deviations)
        )
    workload_manifest = load_workloads()
    workloads = select_workloads(workload_manifest, options.workload_names)
    commands = plan(options, manifest, workloads)
    if dry_run:
        for deviation in deviations:
            print(f"deviation: {deviation}")
        for command in commands:
            print(f"# {command.description}\n{command.shell()}")
        return None

    started = datetime.now(UTC).isoformat(timespec="seconds")
    options.out_dir.mkdir(parents=True, exist_ok=True)
    for command in commands:
        execute(command)
    finished = datetime.now(UTC).isoformat(timespec="seconds")

    thresholds = thresholds_from(manifest, options.iterations)
    sides = read_sides(options, thresholds)
    control = build_control(
        workload_manifest["control_workload"], read_controls(options, thresholds), thresholds
    )
    run_id, run_url, attempt = github_run()
    run = RunInfo(
        id=run_id,
        url=run_url,
        attempt=attempt,
        lane=lane.name,
        runner_label=options.runner_label or (lane.shared_runner if options.advisory else lane.runner_label),
        advisory=options.advisory,
        publishable=not options.advisory and not deviations,
        deviations=deviations,
        started_at=started,
        finished_at=finished,
        helios_git_sha=git_sha(),
    )
    return assemble_report(
        workloads=workloads,
        sides=sides,
        control=control,
        run=run,
        hardware=collect_hardware(lane),
        pins=collect_pins(lane, workloads),
        thresholds=thresholds,
    )
