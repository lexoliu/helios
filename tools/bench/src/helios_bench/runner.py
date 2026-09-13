"""Drives one benchmark run on one lane and assembles its report.

The harness under tools/wasi-apps already knows how to boot Helios,
provision the pinned Fedora guest, and time every side; this module only
decides what to run, refuses hosts that deviate from the lane, records
every pin, and turns the raw JSONL into a report.
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import shlex
import subprocess
import time
from collections.abc import Callable
from dataclasses import dataclass, field, replace
from datetime import UTC, datetime
from pathlib import Path

from helios_bench import REPO_ROOT, WASI_APPS_ROOT
from helios_bench.assemble import assemble_report, build_cell, build_control
from helios_bench.baseline import Baseline
from helios_bench.baseline import prepare as prepare_baseline
from helios_bench.gate import evaluate_paired
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
from helios_bench.report import (
    Control,
    Hardware,
    NoiseRetry,
    Pins,
    Report,
    RetriedForNoise,
    RetrySkippedForBudget,
    RunInfo,
    Side,
    Thresholds,
)
from helios_bench.sources import (
    SIDE_CONTROL_JSONL,
    RawSide,
    read_control,
    read_optional_side,
)
from helios_bench.wasi_apps import workload_runner
from helios_bench.workloads import load_workloads, select_workloads

GAP_BENCH = WASI_APPS_ROOT / "linux-gap-bench.py"
NATIVE_BUILD = REPO_ROOT / "tools" / "bench" / "native" / "build.sh"
NATIVE_ARTIFACTS = REPO_ROOT / "artifacts" / "bench-native"
BOOT_ARTIFACTS = WASI_APPS_ROOT / "boot-artifacts.toml"
CARGO_TARGETS = {"aarch64": "aarch64-unknown-none", "x86-64": "x86_64-unknown-none"}
HELIOS_OUT = "helios"
HELIOS_BASELINE_OUT = "helios-baseline"
RETAKE_OUT = "retake"
RECONFIRM_OUT = "reconfirm"
RETRY_OUT = "retry"
LINUX_OUT = "linux"
LINUX_SIDES = {Side.LINUX_NATIVE, Side.LINUX_WASMTIME}
# The cargo profiles a Helios image of a run can be built with, as the
# inspector names them. The baseline image is always the plain one: a
# pairing varies the candidate.
RELEASE_BUILD = "release"
PROFILE_USE_BUILD = "profile-use"
# Architectures whose `--release` kernel is built against the profile the
# latest release published (docs/pgo.md, #226). On those there is no plain
# release kernel: the inspector compiles one with `-C profile-use` and its
# artifacts land in the `profile-use` directory, so the run record and the
# bootfs pins below have to say so.
RELEASE_PROFILE_ARCHS = frozenset({"x86-64"})
# Where `helios-cli profile-fetch` records which profile this checkout
# builds against.
KERNEL_PROFILE_RECORD = REPO_ROOT / "target" / "profiles" / "fetched.json"
# Which subdirectory of the run's output each side's raw JSONL lands in.
# The two Helios images write the same file names, so the directory is
# what tells their records apart.
SIDE_OUT = {
    Side.HELIOS: HELIOS_OUT,
    Side.HELIOS_BASELINE: HELIOS_BASELINE_OUT,
    Side.LINUX_NATIVE: LINUX_OUT,
    Side.LINUX_WASMTIME: LINUX_OUT,
}


@dataclass(frozen=True)
class NetworkOptions:
    ifname: str | None = None
    bridge: str | None = None
    queues: int | None = None
    reuse_host_listeners: bool = False


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
    # The whole Helios side of the lane, control runs included. The
    # per-class cap above is what a healthy class may take; this is what
    # all of them together may take, so a class whose guest stops
    # answering cannot spend the job.
    helios_side_timeout_seconds: int = 10800
    # Workloads to leave out of the Linux side. A cell whose Helios half
    # cannot be measured has nothing to compare a Linux number against,
    # and the Linux side's budget is shared out between the workloads it
    # runs, so an uncomparable one takes its share from the rest.
    skip_linux_workloads: tuple[str, ...] = ()
    linux_setup_timeout_seconds: int = 5400
    # The timeout of the job this run executes inside, in minutes —
    # `bench-suite.yml` passes its own `timeout-minutes`. The one retry of
    # an inconclusive paired control is sized against it: a second pass
    # that would outlast what remains is skipped rather than killed
    # mid-flight, and the run record says so. None bounds nothing: a local
    # run has no job to outlast.
    job_timeout_minutes: int | None = None
    network: NetworkOptions = NetworkOptions()
    # The second Helios image this run is timed against, or None for an
    # ordinary run. Its presence adds the `helios_baseline` side and makes
    # the Helios half boot both images for every workload. The side
    # timeout above still bounds that half as a whole: a paired run
    # shares it out over twice as many boots, and each of those boots
    # carries one workload rather than a whole class.
    baseline: Baseline | None = None
    # The merged `.profdata` the candidate kernel is compiled against
    # (docs/pgo.md). It pairs the same way a baseline commit does, and
    # against the plain release build of this checkout: what varies
    # between the columns is the profile, not the source.
    profile_use: Path | None = None
    # The baseline built without the fetched kernel profile: the plain
    # control of a PGO measurement (docs/pgo.md, #322). Alone, it pairs
    # this checkout's profile-guided release kernel against its plain one;
    # with a baseline commit, that commit's plain kernel.
    plain_baseline: bool = False

    def __post_init__(self) -> None:
        if self.plain_baseline and not self.reads_release_profile:
            raise SystemExit(
                f"--baseline-kernel-build {RELEASE_BUILD} is the control of a PGO measurement, "
                f"and a release kernel of lane {self.lane.name} ({self.lane.helios_arch}) reads "
                "no profile: its plain build is the only build (docs/pgo.md)"
            )

    @property
    def paired(self) -> bool:
        """Whether this run times a second Helios image beside the first."""
        return self.baseline is not None or self.profile_use is not None or self.plain_baseline

    @property
    def reads_release_profile(self) -> bool:
        """Whether a plain release build of this lane reads a profile."""
        return self.lane.helios_arch in RELEASE_PROFILE_ARCHS

    @property
    def kernel_build(self) -> str:
        """The cargo profile the candidate kernel is built with."""
        if self.profile_use or self.reads_release_profile:
            return PROFILE_USE_BUILD
        return RELEASE_BUILD

    @property
    def baseline_kernel_build(self) -> str | None:
        """The cargo profile the second image is built with, if there is one.

        The baseline image is built the way a `--release` build of this
        lane is, which on an architecture whose release builds read a
        profile is itself a `profile-use` build: what separates the two
        columns of a PGO pairing is then the profile, not the build kind.
        The plain control (`--baseline-kernel-build release`) is the one
        baseline built without a profile on such a lane.
        """
        if not self.paired:
            return None
        if self.plain_baseline:
            return RELEASE_BUILD
        return PROFILE_USE_BUILD if self.reads_release_profile else RELEASE_BUILD


@dataclass(frozen=True)
class PlannedCommand:
    description: str
    argv: list[str]
    env: dict[str, str]
    cwd: Path
    #: Whether this invocation is the suite's Helios pass — the one the
    #: retry of an inconclusive paired control repeats, whose wall time is
    #: the estimate that retry is sized against.
    helios_pass: bool = False

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


def driver_arguments(options: RunOptions, iterations: int, workloads: list[dict], control: bool) -> list[str]:
    """The driver argv every side shares: the iteration count, the workload
    selection, and the control pass when the invocation is the suite."""
    lane = options.lane
    arguments = ["python3", str(GAP_BENCH), "--iterations", str(iterations)]
    if control:
        arguments.append("--control")
    arguments.extend(
        [
            # Every cell of the report is accounted for: a workload that fails
            # is recorded as failed on that side and the run goes on.
            "--keep-going",
            "--helios-host-http-host",
            lane.net_host,
            "--helios-host-tcp-host",
            lane.net_host,
        ]
    )
    if options.allow_busy_host:
        arguments.append("--allow-busy-host")
    if options.network.reuse_host_listeners:
        arguments.append("--reuse-host-listeners")
    for workload in workloads:
        arguments.extend(["--workload", workload["name"]])
    return arguments


def helios_command(
    options: RunOptions,
    common: list[str],
    out_root: Path,
    description: str,
    helios_pass: bool = False,
) -> PlannedCommand:
    """The driver invocation that times the Helios image, and the baseline
    image beside it when the run is paired, under ``out_root``."""
    lane = options.lane
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
    return PlannedCommand(
        description=description,
        argv=[
            *common,
            "--arch",
            lane.helios_arch,
            "--helios-accel",
            lane.accelerator,
            "--skip-linux",
            "--helios-timeout-seconds",
            str(options.helios_timeout_seconds),
            "--helios-side-timeout-seconds",
            str(options.helios_side_timeout_seconds),
            "--out-dir",
            str(out_root / HELIOS_OUT),
            *baseline_arguments(options, out_root),
        ],
        env=env,
        cwd=REPO_ROOT,
        helios_pass=helios_pass,
    )


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
    common = driver_arguments(options, iterations, workloads, control=True)
    if Side.HELIOS in options.sides:
        commands.append(
            helios_command(
                options,
                common,
                options.out_dir,
                "time every workload on Helios",
                helios_pass=True,
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
                    *[
                        argument
                        for name in options.skip_linux_workloads
                        for argument in ("--skip-workload", name)
                    ],
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


def fetched_kernel_profile_label() -> str:
    """The profile this checkout's release kernel was built against.

    `helios-cli profile-fetch` writes the record and the inspector
    refuses a release build without it, so a run that has already booted
    a guest has one; a missing record is a run that never built what it
    says it built. The label is the record's own: `release <tag>` for a
    release's asset, or the branch, commit and run of a `kernel-profile.yml`
    collection (docs/pgo.md, #313).
    """
    if not KERNEL_PROFILE_RECORD.is_file():
        raise SystemExit(
            f"{KERNEL_PROFILE_RECORD} is not there, so this lane's release kernel was not built "
            "against a fetched profile: run `helios-cli profile-fetch` (docs/pgo.md)"
        )
    record = json.loads(KERNEL_PROFILE_RECORD.read_text())
    source = record["source"]
    if source == "release":
        return f"release {record['tag']}"
    if source == "collection":
        return f"{record['head_branch']}@{record['head_sha'][:7]} run {record['run_id']}"
    raise SystemExit(f"{KERNEL_PROFILE_RECORD} names a profile source {source!r} this tool does not know")


def profile_label(path: Path) -> str:
    """A profile named short enough for a table cell."""
    try:
        return str(path.relative_to(REPO_ROOT))
    except ValueError:
        return str(path)


def kernel_profiles(options: RunOptions) -> tuple[str | None, str | None]:
    """Which profile each column's kernel was built against.

    The candidate reads the profile the run named, and otherwise the
    release's on a lane whose release builds read one. The baseline image
    is built plain, so it reads the release's or none — which is exactly
    what a PGO pairing measures once every release publishes a profile:
    the release's counts against a freshly collected set (#226).
    """
    fetched = fetched_kernel_profile_label() if options.reads_release_profile else None
    candidate = profile_label(options.profile_use) if options.profile_use else fetched
    if not options.paired or options.plain_baseline:
        return candidate, None
    return candidate, fetched


def baseline_arguments(options: RunOptions, out_root: Path) -> list[str]:
    """What the driver needs to time the second image beside the first.

    Either axis of a pairing names the same second output directory: the
    two images write the same file names and the directory is what tells
    their records apart.
    """
    arguments = []
    if options.profile_use is not None:
        arguments.extend(["--helios-profile-use", str(options.profile_use)])
    if options.baseline is not None:
        arguments.extend(["--helios-baseline-root", str(options.baseline.worktree)])
    if options.plain_baseline:
        arguments.append("--helios-baseline-without-kernel-profile")
    if not arguments:
        return []
    return [*arguments, "--helios-baseline-out-dir", str(out_root / HELIOS_BASELINE_OUT)]


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


#: The first two lines of a `<kernel>.pgo-uncovered.txt`: the figures the
#: build wrote (`# uncovered: <n> of <functions>` and `# warnings
#: emitted: <m>`).
PGO_UNCOVERED_HEADER = re.compile(r"# uncovered:\s*(\d+)\s+of\s+(\d+)\s+functions")
PGO_WARNINGS_HEADER = re.compile(r"# warnings emitted:\s*(\d+)")


def inspector_for(workspace_root: Path) -> Path:
    """The `helios-inspector` the checkout at ``workspace_root`` compiles.

    A question about a workspace's kernel artifacts goes to that
    workspace's own tooling: a paired run builds and boots each image
    with the `helios-inspector`/`helios-cli` of its own ref (#356), so a
    baseline worktree's kernel is asked of the baseline's inspector under
    its `target/release`. `HELIOS_INSPECTOR_BIN` names this checkout's
    inspector and no other — it is the pin a run sets for the candidate.
    """
    if workspace_root.resolve() == REPO_ROOT.resolve():
        return Path(
            os.environ.get("HELIOS_INSPECTOR_BIN", workspace_root / "target" / "release" / "helios-inspector")
        )
    return workspace_root / "target" / "release" / "helios-inspector"


def kernel_pgo_uncovered(
    workspace_root: Path,
    lane: Lane,
    profile_use: Path | None,
) -> tuple[int, int] | None:
    """The uncovered/function counts a Helios image's kernel build recorded.

    The inspector counts the `no profile data available for function`
    warnings of a profile-use build and writes them beside the kernel as
    `<kernel>.pgo-uncovered.txt`, headed by the image functions a warning
    named — `<uncovered> of <functions>`, the functions the image defines
    (docs/pgo.md). The kernel's own path is asked of the inspector rather
    than rebuilt here: the mapping from architecture and profile to
    target directory is the inspector's, and a second copy of it in this
    file would be a second thing to keep true. `None` when the build kept
    no list — a plain release build, a target whose releases read no
    profile, or a kernel built before the list existed. A `kernel-path`
    that fails is not that case: it is a broken inspector, a wrong
    workspace root, or a refused profile, and it is fatal rather than a
    missing count.
    """
    inspector = inspector_for(workspace_root)
    argv = [
        str(inspector),
        "vm",
        "--arch",
        lane.helios_arch,
        "--release",
        "--accel",
        lane.accelerator,
    ]
    if profile_use is not None:
        argv += ["--profile-use", str(profile_use)]
    argv.append("kernel-path")
    env = os.environ.copy()
    env["HELIOS_WORKSPACE_ROOT"] = str(workspace_root)
    completed = subprocess.run(argv, cwd=REPO_ROOT, env=env, capture_output=True, text=True, check=False)
    if completed.returncode != 0:
        raise SystemExit(
            f"`{shlex.join(argv)}` exited with status {completed.returncode}: {completed.stderr.strip()}"
        )
    listing = Path(completed.stdout.strip() + ".pgo-uncovered.txt")
    if not listing.is_file():
        return None
    with listing.open("r", encoding="utf-8") as handle:
        first, second = handle.readline(), handle.readline()
    match, warnings = PGO_UNCOVERED_HEADER.match(first), PGO_WARNINGS_HEADER.match(second)
    if match is None or warnings is None:
        raise SystemExit(
            f"{listing} does not open with its `# uncovered:`/`# warnings emitted:` "
            f"lines: {first!r} {second!r}"
        )
    listed = sum(
        1 for line in listing.open("r", encoding="utf-8") if line.strip() and not line.startswith("#")
    )
    if listed != int(warnings.group(1)):
        raise SystemExit(f"{listing} names {warnings.group(1)} emitted warnings but lists {listed}")
    return int(match.group(1)), int(match.group(2))


def bootfs_cwasm_digests(lane: Lane, kernel_build: str) -> dict[str, str]:
    """SHA256 of the signed cwasm files the Helios guest loaded.

    Under the candidate's own build directory: the inspector's prebuild
    writes beside the kernel it prebuilds for, so a PGO candidate's
    bootfs is not in the plain release directory.
    """
    prebuild = REPO_ROOT / "target" / "kernel-prebuild" / CARGO_TARGETS[lane.helios_arch] / kernel_build
    if not prebuild.is_dir():
        return {}
    return {path.name: sha256_of(path) for path in sorted(prebuild.glob("*.cwasm"))}


def collect_pins(lane: Lane, workloads: list[dict], kernel_build: str) -> Pins:
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
        bootfs_cwasm=bootfs_cwasm_digests(lane, kernel_build),
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
        out_dir = options.out_dir / SIDE_OUT[side]
        raw = read_optional_side(out_dir, side, thresholds.warmup_discard)
        if raw is None:
            raise SystemExit(f"the {side} side produced no JSONL under {out_dir}")
        sides[side] = raw
    return sides


def read_controls(options: RunOptions, thresholds: Thresholds) -> dict[Side, tuple[RawSide, RawSide]]:
    controls = {}
    for side in options.sides:
        out_dir = options.out_dir / SIDE_OUT[side]
        before = read_control(out_dir, side, "before", thresholds.warmup_discard)
        after = read_control(out_dir, side, "after", thresholds.warmup_discard)
        if before is not None and after is not None:
            controls[side] = (before, after)
    return controls


HELIOS_SIDES = frozenset({Side.HELIOS, Side.HELIOS_BASELINE})


def dispersed_headline_workloads(
    sides: dict[Side, RawSide], workloads: list[dict], thresholds: Thresholds
) -> list[str]:
    """The headline workloads whose Helios or baseline cell the gate would
    reject for dispersion, in manifest order."""
    dispersed = []
    for workload in workloads:
        if not workload.get("headline", False):
            continue
        for side in HELIOS_SIDES:
            raw = sides.get(side)
            raw_cell = raw.cells.get(workload["name"]) if raw is not None else None
            if raw_cell is None or raw_cell.failure is not None:
                continue
            if build_cell(side, raw_cell.iterations, thresholds).rejected:
                dispersed.append(workload["name"])
                break
    return dispersed


def retake_plan(
    options: RunOptions, iterations: int, names: list[str], out_name: str, reason: str
) -> PlannedCommand:
    """The driver invocation that times the workloads ``names`` again on
    every Helios image the run has, under ``out_name/`` beside the first pass."""
    return helios_command(
        options,
        driver_arguments(options, iterations, [{"name": name} for name in names], control=False),
        options.out_dir / out_name,
        f"measure {', '.join(names)} again on every Helios image: {reason}",
    )


def retake_workloads(
    options: RunOptions,
    iterations: int,
    names: list[str],
    sides: dict[Side, RawSide],
    thresholds: Thresholds,
    out_name: str,
    reason: str,
) -> list[str]:
    """Times the workloads ``names`` again on every Helios image the run has,
    back to back through the same driver, and replaces their cells on every
    image so the pair stays paired. Returns the names measured again."""
    if not names:
        return []
    execute(retake_plan(options, iterations, names, out_name, reason))
    for side in options.sides & HELIOS_SIDES:
        out_dir = options.out_dir / out_name / SIDE_OUT[side]
        raw = read_optional_side(out_dir, side, thresholds.warmup_discard)
        if raw is None:
            raise SystemExit(f"the second pass produced no JSONL for the {side} side under {out_dir}")
        for name in names:
            cell = raw.cells.get(name)
            if cell is None:
                raise SystemExit(
                    f"the second pass of {name} wrote no records for the {side} side under {out_dir}"
                )
            sides[side].cells[name] = cell
    return list(names)


def retake(
    options: RunOptions,
    iterations: int,
    workloads: list[dict],
    sides: dict[Side, RawSide],
    thresholds: Thresholds,
) -> list[str]:
    """Re-measures, once, each headline workload whose cell the gate would
    reject for dispersion.

    A cell past the dispersion bound cannot be trusted to detect a
    regression, and a headline workload without a trustworthy pair blocks
    (run 34390597958: one baseline cell at CV 0.15012 against 0.150 cost
    the whole hour). A retake that is still dispersed stands: a host that
    cannot produce two clean series in a row is the inconclusive case, not
    a loop.
    """
    if Side.HELIOS not in options.sides:
        return []
    return retake_workloads(
        options,
        iterations,
        dispersed_headline_workloads(sides, workloads, thresholds),
        sides,
        thresholds,
        RETAKE_OUT,
        "the first pass was too dispersed to gate on",
    )


def regressed_headline_workloads(report: Report) -> list[str]:
    """The headline workloads the paired gate would block on, in report order."""
    result = evaluate_paired(report)
    if result is None or result.inconclusive:
        return []
    names: list[str] = []
    for row in result.headline_regressions:
        if row.workload not in names:
            names.append(row.workload)
    return names


def reconfirm(
    options: RunOptions,
    iterations: int,
    report: Report,
    sides: dict[Side, RawSide],
    thresholds: Thresholds,
) -> list[str]:
    """Measures, once, every headline workload the paired gate would block on.

    A regression that is the change's own reproduces on a second pair of
    boots; a drift between two boots does not (run 34404354482:
    `sched-tasks` +6.9% on a 6.7% floor, identical kernels). The regressed
    workloads are timed again on both images back to back and the second
    pair replaces the first, so the gate reads a regression that showed
    twice. One pass: a host that drifts twice in a row still fails the check.
    """
    if not report.run.paired:
        return []
    return retake_workloads(
        options,
        iterations,
        regressed_headline_workloads(report),
        sides,
        thresholds,
        RECONFIRM_OUT,
        "the first pair of boots regressed and a regression has to show twice",
    )


def retry(
    options: RunOptions,
    iterations: int,
    workloads: list[dict],
    report: Report,
    first_sides: dict[Side, RawSide],
    thresholds: Thresholds,
    build: Callable[[dict[Side, RawSide], Control | None, list[str], list[str], NoiseRetry | None], Report],
    helios_seconds: float,
    run_started: float,
) -> Report | None:
    """Measures the paired suite a second time when the first pass's control is unreadable.

    An inconclusive control means the host moved by more than any effect a
    change could show while the suite ran, so no row can take a verdict —
    but one noisy stretch says nothing about the same machine an hour later,
    and redispatching would only hope to draw a quieter one. The control
    pair and the suite therefore run once more in the same job, on the same
    host, under ``retry/`` beside the first pass; retake and reconfirm apply
    to the second pass as they did to the first, and the report the gate
    reads is the second pass's. One retry: a second pass still past the
    bound is a host that could not produce a clean control twice, and the
    run fails as it would have without one (#375). An unpaired run has no
    paired verdict to retry and is never re-measured, and neither is a
    first pass whose floor stayed under the bound.

    The second pass re-runs only the Helios pair — the Linux sides'
    first-pass cells stand — so a retry pass needs a baseline
    (``--baseline-ref`` or ``--profile-use``) to compare, which every paired
    run already has. When the run carries a ``--job-timeout-minutes`` the
    retry is sized against it first: a second pass is estimated at the
    first pass's Helios wall time (``helios_seconds``, measured against the
    monotonic ``run_started``), and one that would outlast what the budget
    has left is not started — the first pass's report stands, still
    inconclusive, with a ``skipped-for-budget`` record on the run. And a retry
    pass that comes back without the control pair it was asked to measure
    is a failed pass, not a clean one: the run stops naming the side and
    the files it expected rather than reporting a floor of zero.
    """
    result = evaluate_paired(report)
    if result is None or not result.inconclusive:
        return None
    if options.job_timeout_minutes is not None:
        remaining = options.job_timeout_minutes * 60 - (time.monotonic() - run_started)
        if helios_seconds > remaining:
            print(
                f"the second pass would need {helios_seconds:.0f} s and "
                f"{remaining:.0f} s remain of the job's budget; the run stays inconclusive",
                flush=True,
            )
            return build(
                first_sides,
                report.control,
                report.run.retaken,
                report.run.reconfirmed,
                RetrySkippedForBudget(
                    first_noise_floor=result.noise_floor,
                    needed_seconds=helios_seconds,
                    remaining_seconds=remaining,
                ),
            )
    retry_options = replace(
        options,
        out_dir=options.out_dir / RETRY_OUT,
        sides=options.sides & HELIOS_SIDES,
    )
    execute(
        helios_command(
            retry_options,
            driver_arguments(retry_options, iterations, workloads, control=True),
            retry_options.out_dir,
            "measure the control pair and every workload on both Helios images once more: "
            f"the first pass's noise floor crossed the {result.floor_bound:.3f} bound",
        )
    )
    second_sides = read_sides(retry_options, thresholds)
    retaken = retake(retry_options, iterations, workloads, second_sides, thresholds)
    controls = read_controls(retry_options, thresholds)
    for side in sorted(retry_options.sides):
        out_dir = retry_options.out_dir / SIDE_OUT[side]
        pair = controls.get(side)
        if pair is None:
            raise SystemExit(
                f"the retry pass produced no control pair for the {side} side: "
                f"{out_dir / SIDE_CONTROL_JSONL[side].format(moment='before')} and "
                f"{out_dir / SIDE_CONTROL_JSONL[side].format(moment='after')} were expected"
            )
        for moment, raw in (("before", pair[0]), ("after", pair[1])):
            if report.control.workload not in raw.cells:
                raise SystemExit(
                    f"the retry pass's {side} side control at "
                    f"{out_dir / SIDE_CONTROL_JSONL[side].format(moment=moment)} "
                    f"recorded no `{report.control.workload}` cell"
                )
    control = build_control(report.control.workload, controls, thresholds)
    for side, raw in first_sides.items():
        second_sides.setdefault(side, raw)
    record = RetriedForNoise(
        first_noise_floor=result.noise_floor,
        second_noise_floor=control.noise_floor,
    )
    second = build(second_sides, control, retaken, [], record)
    reconfirmed = reconfirm(retry_options, iterations, second, second_sides, thresholds)
    if reconfirmed:
        second = build(second_sides, control, retaken, reconfirmed, record)
    return second


def run_suite(options: RunOptions, manifest: Manifest, dry_run: bool = False) -> Report | None:
    lane = options.lane
    deviations = host_deviations(lane)
    if options.network.reuse_host_listeners:
        deviations.append(
            "shared host listeners requested for reconnect diagnosis; not performance acceptance"
        )
    if deviations and not options.advisory:
        raise SystemExit(
            "this host deviates from lane "
            f"{lane.name}; refusing to produce a publishable report:\n  - " + "\n  - ".join(deviations)
        )
    paired = options.paired
    if paired and not {Side.HELIOS, Side.HELIOS_BASELINE} <= options.sides:
        raise SystemExit(
            "a paired run times both Helios images: --sides has to name helios and helios_baseline"
        )
    if not paired and Side.HELIOS_BASELINE in options.sides:
        raise SystemExit(
            "the helios_baseline side needs --baseline-ref or --profile-use to say what it is built from"
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

    # The monotonic clock the retry's budget check reads, started before
    # the baseline is prepared and the first command runs: `started`
    # below is the report's wall timestamp.
    run_started = time.monotonic()
    if options.baseline is not None:
        # Before the clock starts: the worktree, the links it shares with
        # the candidate, and the build the driver then does are all fixed
        # costs of the pairing, not of any workload.
        prepare_baseline(options.baseline)

    started = datetime.now(UTC).isoformat(timespec="seconds")
    options.out_dir.mkdir(parents=True, exist_ok=True)
    helios_seconds = 0.0
    for command in commands:
        before = time.monotonic()
        execute(command)
        if command.helios_pass:
            helios_seconds = time.monotonic() - before
    thresholds = thresholds_from(manifest, options.iterations)
    sides = read_sides(options, thresholds)
    retaken = retake(options, thresholds.iterations, workloads, sides, thresholds)
    control = build_control(
        workload_manifest["control_workload"], read_controls(options, thresholds), thresholds
    )
    run_id, run_url, attempt = github_run()

    candidate_profile, baseline_profile = kernel_profiles(options)

    def build(
        sides: dict[Side, RawSide],
        control: Control | None,
        retaken: list[str],
        reconfirmed: list[str],
        noise_retry: NoiseRetry | None = None,
    ) -> Report:
        finished = datetime.now(UTC).isoformat(timespec="seconds")
        # The uncovered/function counts each profile-use image's build
        # left beside its kernel; None where the build kept no list.
        candidate_uncovered = (
            kernel_pgo_uncovered(REPO_ROOT, lane, options.profile_use)
            if options.kernel_build == PROFILE_USE_BUILD
            else None
        )
        baseline_uncovered = (
            kernel_pgo_uncovered(
                options.baseline.worktree if options.baseline is not None else REPO_ROOT,
                lane,
                None,
            )
            if options.baseline_kernel_build == PROFILE_USE_BUILD
            else None
        )
        run = RunInfo(
            id=run_id,
            url=run_url,
            attempt=attempt,
            lane=lane.name,
            runner_label=options.runner_label
            or (lane.shared_runner if options.advisory else lane.runner_label),
            advisory=options.advisory,
            publishable=not options.advisory and not deviations,
            deviations=deviations,
            started_at=started,
            finished_at=finished,
            helios_git_sha=git_sha(),
            # A PGO pairing varies the build and not the commit, so the
            # baseline image is this same commit: the run record says so
            # rather than leaving the column unattributed.
            baseline_git_sha=options.baseline.sha if options.baseline else (git_sha() if paired else None),
            baseline_ref=options.baseline.ref if options.baseline else None,
            # The tooling is the same statement as the kernel's: each
            # side's helios-inspector/helios-cli are built from the commit
            # its guest is built from (#356).
            inspector_git_sha=git_sha(),
            baseline_inspector_git_sha=options.baseline.sha
            if options.baseline
            else (git_sha() if paired else None),
            kernel_build=options.kernel_build,
            baseline_kernel_build=options.baseline_kernel_build,
            kernel_profile=candidate_profile,
            baseline_kernel_profile=baseline_profile,
            kernel_pgo_uncovered=(candidate_uncovered[0] if candidate_uncovered is not None else None),
            kernel_pgo_functions=(candidate_uncovered[1] if candidate_uncovered is not None else None),
            baseline_kernel_pgo_uncovered=(baseline_uncovered[0] if baseline_uncovered is not None else None),
            baseline_kernel_pgo_functions=(baseline_uncovered[1] if baseline_uncovered is not None else None),
            retaken=retaken,
            reconfirmed=reconfirmed,
            noise_retry=noise_retry,
        )
        return assemble_report(
            workloads=workloads,
            sides=sides,
            control=control,
            run=run,
            hardware=collect_hardware(lane),
            pins=collect_pins(lane, workloads, options.kernel_build),
            thresholds=thresholds,
        )

    report = build(sides, control, retaken, [])
    reconfirmed = reconfirm(options, thresholds.iterations, report, sides, thresholds)
    if reconfirmed:
        report = build(sides, control, retaken, reconfirmed)
    retried = retry(
        options,
        thresholds.iterations,
        workloads,
        report,
        sides,
        thresholds,
        build,
        helios_seconds,
        run_started,
    )
    return retried or report
