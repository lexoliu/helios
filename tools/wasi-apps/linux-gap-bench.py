#!/usr/bin/env python3
import argparse
import html
import http.server
import json
import os
import platform
import shutil
import signal
import shlex
import socketserver
import subprocess
import sys
import threading
import time
import tomllib
from pathlib import Path

import linux_workload_runner as runner
from fedora_qemu_baseline import (
    DEFAULT_DISK_SIZE,
    DEFAULT_MEMORY,
    DEFAULT_SMP,
    FEDORA_IMAGE_SHA256,
    FEDORA_IMAGE_URLS,
    QEMU_BINS,
    default_asset_dir,
    host_arch,
    wasm_uses_simd as fedora_wasm_uses_simd,
    run_fedora_qemu_linux,
)

# Helios inspector arch name -> Fedora guest arch name.
LINUX_GUEST_ARCHES = {"aarch64": "aarch64", "x86-64": "x86_64"}
from tcp_echo_server import start_tcp_echo_server
from tcp_throughput_server import DEFAULT_PAYLOAD_BYTES, start_tcp_throughput_server

# Workload classes in the order the report lists them; each names the design
# claim its workloads isolate (see docs/benchmarks.md).
WORKLOAD_CLASSES = ["startup", "hostcall", "ipc", "sched", "net", "fs", "compute"]

HTTP_PAYLOAD_FILE = "payload.txt"
HTTP_LARGE_PAYLOAD_FILE = "payload-64m.bin"
HTTP_PAYLOAD = b"helios-linux-gap:ok\n"
HTTP_LARGE_PAYLOAD_BYTES = DEFAULT_PAYLOAD_BYTES
HTTP_LARGE_PAYLOAD_CHUNK = bytes(range(251))
# Where the host-side HTTP and TCP servers listen.
#
# Which host address the guest dials is a property of the packet path
# its virtio-net device sits on, not of this script. Slirp translates
# the guest's 10.0.2.2 into the host's loopback, so a loopback-bound
# server answers. A tap puts the guest on a real bridge, where the
# address it dials is the bridge's own; a loopback-bound server is not
# listening there, the host kernel answers the SYN with a RST, and the
# workload reports `ErrorCode::ConnectionRefused`. Listening on every
# address is the one bind that serves both paths. The ports are
# ephemeral and the servers live only for the run.
HOST_SERVER_BIND_ADDRESS = "0.0.0.0"
DEFAULT_MAX_HOST_LOAD_PER_CPU = 0.75
TOP_CPU_PROCESS_LIMIT = 8
DEFAULT_HELIOS_TIMEOUT_SECONDS = 600
STALE_PROCESS_COMMAND_LIMIT = 240


class QuietHttpHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, format: str, *args: object) -> None:
        return


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def run(command: list[str], env: dict[str, str] | None = None) -> None:
    subprocess.run(command, cwd=repo_root(), env=env, check=True)


def terminate_process_group(pid: int) -> None:
    try:
        os.killpg(pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    except PermissionError:
        return
    deadline = time.monotonic() + 2.0
    while time.monotonic() < deadline:
        try:
            os.killpg(pid, 0)
        except ProcessLookupError:
            return
        except PermissionError:
            return
        time.sleep(0.05)
    try:
        os.killpg(pid, signal.SIGKILL)
    except ProcessLookupError:
        return
    except PermissionError:
        return


class HeliosRunFailed(RuntimeError):
    """One `workload-bench.sh` invocation did not finish its workloads.

    A guest that dies costs the invocation it was serving and nothing
    else: the suite boots one VM per workload class, so the caller
    records the class it lost and keeps the classes it can still measure.
    """


def run_isolated(
    command: list[str],
    env: dict[str, str] | None = None,
    timeout_seconds: int = DEFAULT_HELIOS_TIMEOUT_SECONDS,
) -> None:
    process = subprocess.Popen(
        command,
        cwd=repo_root(),
        env=env,
        start_new_session=True,
    )
    try:
        returncode = process.wait(timeout=timeout_seconds)
    except subprocess.TimeoutExpired as error:
        terminate_process_group(process.pid)
        raise HeliosRunFailed(
            f"command timed out after {timeout_seconds}s: {shlex.join(command)}"
        ) from error
    finally:
        terminate_process_group(process.pid)
    if returncode != 0:
        raise HeliosRunFailed(f"{shlex.join(command)} exited with status {returncode}")


def output(command: list[str]) -> str:
    completed = subprocess.run(
        command,
        cwd=repo_root(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=True,
    )
    return completed.stdout.strip()


def load_manifest(path: Path) -> dict:
    return runner.load_manifest(path)


def selected_workloads(
    manifest: dict,
    classes: list[str],
    names: list[str],
    skipped: list[str] | None = None,
) -> list[dict]:
    skipped = skipped or []
    known = {workload["name"] for workload in manifest["workloads"]}
    for name in skipped:
        if name not in known:
            raise SystemExit(f"unknown skipped workload {name}")
    selected = []
    for workload in manifest["workloads"]:
        if workload["name"] in skipped:
            continue
        if names and workload["name"] not in names:
            continue
        if classes and workload["class"] not in classes:
            continue
        selected.append(workload)
    if not selected:
        raise SystemExit("workload selection matched no manifest entries")
    for name in names:
        if not any(workload["name"] == name for workload in selected):
            raise SystemExit(f"unknown or filtered workload {name}")
    return selected


def git_short_sha() -> str:
    return output(["git", "rev-parse", "--short", "HEAD"])


def git_sha() -> str:
    return output(["git", "rev-parse", "HEAD"])


def host_memory() -> str:
    if sys.platform == "darwin":
        try:
            bytes_total = int(output(["sysctl", "-n", "hw.memsize"]))
            return f"{bytes_total // (1024 * 1024)} MiB"
        except subprocess.CalledProcessError:
            return "unknown"
    meminfo = Path("/proc/meminfo")
    if meminfo.exists():
        for line in meminfo.read_text(encoding="utf-8").splitlines():
            if line.startswith("MemTotal:"):
                return " ".join(line.split()[1:])
    return "unknown"


def host_cpu() -> str:
    if sys.platform == "darwin":
        try:
            return output(["sysctl", "-n", "machdep.cpu.brand_string"])
        except subprocess.CalledProcessError:
            pass
    return platform.processor() or platform.machine()


def top_cpu_processes(limit: int = TOP_CPU_PROCESS_LIMIT) -> list[dict]:
    completed = subprocess.run(
        ["ps", "-axo", "pid=,pcpu=,command="],
        cwd=repo_root(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    processes = []
    for line in completed.stdout.splitlines():
        parts = line.strip().split(None, 2)
        if len(parts) < 3:
            continue
        try:
            pcpu = float(parts[1])
        except ValueError:
            continue
        processes.append(
            {
                "pid": parts[0],
                "pcpu": pcpu,
                "command": parts[2][:160],
            }
        )
    processes.sort(key=lambda process: process["pcpu"], reverse=True)
    return processes[:limit]


def process_table() -> list[dict]:
    completed = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,command="],
        cwd=repo_root(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    processes = []
    for line in completed.stdout.splitlines():
        parts = line.strip().split(None, 2)
        if len(parts) < 3:
            continue
        processes.append(
            {
                "pid": parts[0],
                "ppid": parts[1],
                "command": parts[2],
            }
        )
    return processes


def is_stale_helios_benchmark_process(command: str) -> bool:
    argv0 = command.split(None, 1)[0] if command.strip() else ""
    executable = Path(argv0).name
    inspector_workload = (
        executable == "helios-inspector"
        and " vm " in f" {command} "
        and " workload-bench" in f" {command} "
    )
    qemu_workload = executable.startswith("qemu-system-") and "helios-inspector-vm." in command
    return inspector_workload or qemu_workload


def enforce_no_stale_helios_benchmark_processes() -> None:
    current_pid = str(os.getpid())
    stale = [
        process
        for process in process_table()
        if process["pid"] != current_pid
        and is_stale_helios_benchmark_process(process["command"])
    ]
    if not stale:
        return
    details = "; ".join(
        f"{process['pid']} ppid={process['ppid']} {process['command'][:STALE_PROCESS_COMMAND_LIMIT]}"
        for process in stale[:5]
    )
    raise SystemExit(
        "refusing to start Helios benchmark while stale Helios VM workload processes exist: "
        f"{details}"
    )


def host_load_snapshot() -> dict:
    cpu_count = os.cpu_count() or 1
    try:
        load1, load5, load15 = os.getloadavg()
        load = {
            "one_minute": load1,
            "five_minutes": load5,
            "fifteen_minutes": load15,
            "one_minute_per_cpu": load1 / cpu_count,
        }
    except OSError:
        load = {
            "one_minute": None,
            "five_minutes": None,
            "fifteen_minutes": None,
            "one_minute_per_cpu": None,
        }
    return {
        "cpu_count": cpu_count,
        "load": load,
        "top_cpu_processes": top_cpu_processes(),
    }


def enforce_host_load(snapshot: dict, max_load_per_cpu: float, allow_busy_host: bool) -> None:
    load_per_cpu = snapshot["load"]["one_minute_per_cpu"]
    if load_per_cpu is None or allow_busy_host or load_per_cpu <= max_load_per_cpu:
        return
    top = ", ".join(
        f"{process['pid']}:{process['pcpu']:.1f}% {process['command']}"
        for process in snapshot["top_cpu_processes"][:3]
    )
    raise SystemExit(
        f"host is too busy for benchmark evidence: 1m load/cpu={load_per_cpu:.2f} "
        f"> {max_load_per_cpu:.2f}; top CPU: {top}; rerun with --allow-busy-host to record noisy diagnostics"
    )


def format_load(value: float | None) -> str:
    return "unknown" if value is None else f"{value:.2f}"


def start_host_http(root: Path) -> tuple[socketserver.TCPServer, int]:
    handler = lambda *args, **kwargs: QuietHttpHandler(*args, directory=str(root), **kwargs)
    server = socketserver.TCPServer((HOST_SERVER_BIND_ADDRESS, 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, int(server.server_address[1])


def write_http_payloads(root: Path) -> None:
    (root / HTTP_PAYLOAD_FILE).write_bytes(HTTP_PAYLOAD)
    large_path = root / HTTP_LARGE_PAYLOAD_FILE
    remaining = HTTP_LARGE_PAYLOAD_BYTES
    with large_path.open("wb") as handle:
        while remaining:
            chunk = HTTP_LARGE_PAYLOAD_CHUNK[: min(remaining, len(HTTP_LARGE_PAYLOAD_CHUNK))]
            handle.write(chunk)
            remaining -= len(chunk)


def run_helios(
    manifest: Path,
    out_dir: Path,
    iterations: int,
    workloads: list[dict],
    arch: str,
    accel: str | None,
    host_http_url: str | None,
    host_tcp_host: str | None,
    host_tcp_port: int | None,
    host_tcp_echo_port: int | None,
    timeout_seconds: int,
    control_workload: dict | None,
    keep_going: bool,
) -> Path:
    log = out_dir / "helios.jsonl"
    workloads_by_class: dict[str, list[str]] = {}
    for workload in workloads:
        workloads_by_class.setdefault(workload["class"], []).append(workload["name"])

    def run_control(moment: str) -> None:
        # The control workload measures the machine, not Helios: the same
        # program before and after the suite bounds how much the host
        # drifted while the numbers in between were taken. It is the run's
        # own precondition, so losing it fails the run.
        if control_workload is None:
            return
        run_helios_once(
            manifest,
            out_dir / f"helios-control-{moment}.jsonl",
            iterations,
            [],
            [control_workload["name"]],
            arch,
            accel,
            host_http_url,
            host_tcp_host,
            host_tcp_port,
            host_tcp_echo_port,
            timeout_seconds,
            keep_going,
        )

    try:
        run_control("before")
    except HeliosRunFailed as error:
        raise SystemExit(f"the control workload could not be measured: {error}") from error

    if len(workloads_by_class) > 1:
        class_logs = []
        lost_classes = []
        for workload_class, workload_names in workloads_by_class.items():
            class_log = out_dir / f"helios-{workload_class}.jsonl"
            try:
                run_helios_once(
                    manifest,
                    class_log,
                    iterations,
                    [workload_class],
                    workload_names,
                    arch,
                    accel,
                    host_http_url,
                    host_tcp_host,
                    host_tcp_port,
                    host_tcp_echo_port,
                    timeout_seconds,
                    keep_going,
                )
            except HeliosRunFailed as error:
                if not keep_going:
                    raise
                # One class runs in one guest, so a guest this class killed
                # costs this class and no other. Whatever it managed to
                # write stays; the workloads it never reached become failed
                # cells naming the reason, and the next class gets a fresh
                # guest.
                print(
                    f"helios class {workload_class!r} did not finish; recording its "
                    f"unmeasured workloads as failed and continuing: {error}",
                    file=sys.stderr,
                    flush=True,
                )
                record_unmeasured(class_log, workloads, workload_names, str(error))
                lost_classes.append(workload_class)
            class_logs.append(class_log)
        with log.open("w", encoding="utf-8") as output_handle:
            for class_log in class_logs:
                if class_log.exists():
                    output_handle.write(class_log.read_text(encoding="utf-8"))
        try:
            run_control("after")
        except HeliosRunFailed as error:
            raise SystemExit(f"the control workload could not be measured: {error}") from error
        if lost_classes:
            print(
                "helios workload classes recorded as failed: " + ", ".join(lost_classes),
                file=sys.stderr,
                flush=True,
            )
        return log

    try:
        run_helios_once(
            manifest,
            log,
            iterations,
            list(workloads_by_class),
            [workload["name"] for workload in workloads],
            arch,
            accel,
            host_http_url,
            host_tcp_host,
            host_tcp_port,
            host_tcp_echo_port,
            timeout_seconds,
            keep_going,
        )
    except HeliosRunFailed as error:
        if not keep_going:
            raise
        print(
            f"helios run did not finish; recording its unmeasured workloads as failed: {error}",
            file=sys.stderr,
            flush=True,
        )
        record_unmeasured(
            log,
            workloads,
            [workload["name"] for workload in workloads],
            str(error),
        )
    try:
        run_control("after")
    except HeliosRunFailed as error:
        raise SystemExit(f"the control workload could not be measured: {error}") from error
    return log


def record_unmeasured(
    log: Path,
    workloads: list[dict],
    selected: list[str],
    error: str,
) -> None:
    """Appends a failure record for every selected workload the log misses.

    A workload with no record at all disappears from the report, which is
    the one outcome a published table must never have: a cell is either a
    number or a stated failure.
    """
    measured = set()
    if log.exists():
        with log.open("r", encoding="utf-8") as handle:
            for line in handle:
                line = line.strip()
                if not line:
                    continue
                record = json.loads(line)
                if record["type"] in ("summary", "failure"):
                    measured.add(record["workload"])
    by_name = {workload["name"]: workload for workload in workloads}
    with log.open("a", encoding="utf-8") as handle:
        for name in selected:
            if name in measured:
                continue
            workload = by_name[name]
            handle.write(
                json.dumps(
                    {
                        "type": "failure",
                        "workload": name,
                        "class": workload["class"],
                        "headline": bool(workload.get("headline", False)),
                        "runner": workload["runner"],
                        "error": error,
                    }
                )
                + "\n"
            )


def run_helios_once(
    manifest: Path,
    log: Path,
    iterations: int,
    classes: list[str],
    names: list[str],
    arch: str,
    accel: str | None,
    host_http_url: str | None,
    host_tcp_host: str | None,
    host_tcp_port: int | None,
    host_tcp_echo_port: int | None,
    timeout_seconds: int,
    keep_going: bool,
) -> None:
    env = os.environ.copy()
    env["HELIOS_WORKLOAD_BENCH_ARCH"] = arch
    if accel:
        # The inspector requires the profile's native accelerator when
        # nobody names one, so the lane's choice is passed down rather
        # than rediscovered per boot.
        env["HELIOS_WORKLOAD_BENCH_ACCEL"] = accel
    env["HELIOS_WORKLOAD_BENCH_ITERATIONS"] = str(iterations)
    env["HELIOS_WORKLOAD_BENCH_MANIFEST"] = str(manifest)
    env["HELIOS_WORKLOAD_BENCH_LOG"] = str(log)
    env["HELIOS_WORKLOAD_BENCH_KERNEL_PROFILE_OUTPUT"] = str(log.with_suffix(".kernel.folded"))
    env["HELIOS_WORKLOAD_BENCH_PERF_METRICS_OUTPUT"] = str(log.with_suffix(".perf.json"))
    runtime_dir = env.get("HELIOS_WORKLOAD_BENCH_RUNTIME_DIR")
    if runtime_dir:
        # One runtime directory per boot. Each workload class is its own
        # VM, and a shared directory means the second one's QEMU log,
        # guest console and raw debug-serial capture overwrite the
        # first's — so a failure in the earlier boot leaves no evidence.
        env["HELIOS_WORKLOAD_BENCH_RUNTIME_DIR"] = str(Path(runtime_dir) / log.stem)
    if classes:
        env["HELIOS_WORKLOAD_BENCH_CLASSES"] = ",".join(classes)
    if names:
        env["HELIOS_WORKLOAD_BENCH_WORKLOADS"] = ",".join(names)
    if host_http_url:
        env["HELIOS_WORKLOAD_BENCH_HOST_HTTP_URL"] = host_http_url
    if host_tcp_host:
        env["HELIOS_WORKLOAD_BENCH_HOST_TCP_HOST"] = host_tcp_host
    if host_tcp_port is not None:
        env["HELIOS_WORKLOAD_BENCH_HOST_TCP_PORT"] = str(host_tcp_port)
    if host_tcp_echo_port is not None:
        env["HELIOS_WORKLOAD_BENCH_HOST_TCP_ECHO_PORT"] = str(host_tcp_echo_port)
    if keep_going:
        env["HELIOS_WORKLOAD_BENCH_KEEP_GOING"] = "1"
    run_isolated(["tools/wasi-apps/workload-bench.sh"], env=env, timeout_seconds=timeout_seconds)


def run_linux(
    manifest: Path,
    out_dir: Path,
    iterations: int,
    workloads: list[dict],
    fedora_image_url: str,
    fedora_image_sha256: str,
    linux_vm_dir: Path,
    linux_vm_qemu_bin: str,
    linux_vm_ssh_port: int | None,
    linux_vm_memory: str,
    linux_vm_smp: int,
    linux_vm_disk_size: str,
    linux_vm_setup_timeout_seconds: int,
    host_http_url: str | None,
    host_tcp_host: str | None,
    linux_tcp_port: int | None,
    linux_tcp_echo_port: int | None,
    quickjs_source_archive: Path | None,
    wasmtime_linux_bin: Path | None,
    wasmtime_linux_archive: Path | None,
    guest_arch: str,
    accel: str | None,
    native_bin_dir: Path | None,
    control_workload: dict | None,
    keep_going: bool,
) -> tuple[Path | None, Path | None, dict]:
    return run_fedora_qemu_linux(
        repo_root(),
        out_dir,
        iterations,
        workloads,
        fedora_image_url,
        fedora_image_sha256,
        linux_vm_dir,
        linux_vm_qemu_bin,
        linux_vm_ssh_port,
        linux_vm_memory,
        linux_vm_smp,
        linux_vm_disk_size,
        linux_vm_setup_timeout_seconds,
        host_http_url,
        host_tcp_host,
        linux_tcp_port,
        linux_tcp_echo_port,
        quickjs_source_archive,
        wasmtime_linux_bin,
        wasmtime_linux_archive,
        guest_arch=guest_arch,
        accel=accel,
        native_bin_dir=native_bin_dir,
        control_workload=control_workload,
        keep_going=keep_going,
    )


def run_wasmtime_profiles(
    manifest: Path,
    out_dir: Path,
    workload_names: list[str],
    mode: str,
    host_http_url: str | None,
    wasmtime_bin: str,
    no_flamegraph: bool,
    guest_interval: str,
    perf_event: str,
) -> list[Path]:
    outputs = []
    for workload_name in workload_names:
        profile_out = out_dir / f"wasmtime-profile-{workload_name}-{mode}"
        command = [
            "tools/wasi-apps/wasmtime-profile.sh",
            "--manifest",
            str(manifest),
            "--workload",
            workload_name,
            "--mode",
            mode,
            "--out-dir",
            str(profile_out),
            "--wasmtime-bin",
            wasmtime_bin,
            "--guest-interval",
            guest_interval,
            "--perf-event",
            perf_event,
        ]
        if host_http_url:
            command.extend(["--host-http-url", host_http_url])
        if no_flamegraph:
            command.append("--no-flamegraph")
        run(command)
        outputs.append(profile_out / "wasmtime-profile.json")
    return outputs


def parse_jsonl(path: Path | None) -> tuple[dict, dict[str, dict]]:
    if path is None or not path.exists():
        return {}, {}
    run_record = {}
    summaries = {}
    with path.open("r", encoding="utf-8") as handle:
        for line in handle:
            if not line.strip():
                continue
            record = json.loads(line)
            if record["type"] == "run":
                run_record = record
            if record["type"] == "summary":
                summaries[record["workload"]] = record
    return run_record, summaries


def parse_linux_jsonl(path: Path | None) -> dict[str, dict]:
    """Per-workload summaries of one Linux side, keyed by workload name.

    ``median`` is kept in seconds because that is the unit every comparison
    below derives its milliseconds from.
    """
    if path is None or not path.exists():
        return {}
    summaries: dict[str, dict] = {}
    metrics: dict[str, list[dict]] = {}
    with path.open("r", encoding="utf-8") as handle:
        for line in handle:
            if not line.strip():
                continue
            record = json.loads(line)
            if record["type"] == "iteration":
                metrics.setdefault(record["workload"], []).append(record["metrics"])
            if record["type"] == "summary":
                summaries[record["workload"]] = {
                    "median": record["median_elapsed_ms"] / 1000.0,
                    "elapsed_ms": record["elapsed_ms"],
                    "metrics": metrics.get(record["workload"], []),
                }
    return summaries


def helios_perf_metric_paths(helios_jsonl: Path | None) -> list[Path]:
    if helios_jsonl is None:
        return []
    return sorted(helios_jsonl.parent.glob("helios*.perf.json"))


def render_helios_kernel_flamegraphs(helios_jsonl: Path | None) -> list[Path]:
    if helios_jsonl is None:
        return []
    profile_paths = sorted(helios_jsonl.parent.glob("helios*.kernel.folded"))
    if not profile_paths:
        return []
    flamegraph = shutil.which("inferno-flamegraph")
    if flamegraph is None:
        raise SystemExit(
            "inferno-flamegraph is required to render Helios kernel flamegraphs; install the `inferno` cargo package"
        )
    outputs = []
    for profile_path in profile_paths:
        output = profile_path.with_suffix(".flamegraph.svg")
        with output.open("w", encoding="utf-8") as handle:
            subprocess.run(
                [flamegraph, str(profile_path)],
                cwd=repo_root(),
                stdout=handle,
                check=True,
            )
        outputs.append(output)
    return outputs


def folded_profile_top_rows(profile_path: Path, limit: int) -> list[dict]:
    rows = []
    total = 0
    with profile_path.open("r", encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            stack, raw_value = line.rsplit(" ", 1)
            value = int(raw_value)
            total += value
            rows.append({"stack": stack, "nanos": value})
    if total == 0:
        return []
    rows.sort(key=lambda row: row["nanos"], reverse=True)
    return [
        {
            "stack": row["stack"],
            "nanos": row["nanos"],
            "percent": row["nanos"] * 100.0 / total,
            "source": profile_path,
        }
        for row in rows[:limit]
    ]


def helios_kernel_profile_top_rows(helios_jsonl: Path | None, limit: int = 12) -> list[dict]:
    if helios_jsonl is None:
        return []
    rows = []
    for profile_path in sorted(helios_jsonl.parent.glob("helios*.kernel.folded")):
        rows.extend(folded_profile_top_rows(profile_path, limit))
    rows.sort(key=lambda row: row["nanos"], reverse=True)
    return rows[:limit]


def parse_perf_metrics(paths: list[Path]) -> list[dict]:
    samples = []
    for path in paths:
        if not path.exists():
            continue
        with path.open("r", encoding="utf-8") as handle:
            for sample in json.load(handle):
                sample = dict(sample)
                sample["_source"] = path
                samples.append(sample)
    return samples


def metric_field(sample: dict, name: str) -> int | str | None:
    return sample.get(name, sample.get(name.replace("_", "-")))


def metric_u64(sample: dict, name: str) -> int:
    value = metric_field(sample, name)
    return int(value) if value is not None else 0


def metric_name(sample: dict) -> str:
    value = metric_field(sample, "name")
    return str(value) if value is not None else ""


def network_perf_rows(paths: list[Path]) -> list[dict]:
    rows = []
    for sample in parse_perf_metrics(paths):
        name = metric_name(sample)
        if not name.startswith("kernel;network;"):
            continue
        total_nanos = metric_u64(sample, "total_nanos")
        total_bytes = metric_u64(sample, "total_bytes")
        total_reference_cycles = metric_u64(sample, "total_reference_cycles")
        rows.append(
            {
                "name": name,
                "events": metric_u64(sample, "total_events"),
                "nanos": total_nanos,
                "bytes": total_bytes,
                "reference_cycles": total_reference_cycles,
                "nanos_per_event": average_or_none(
                    total_nanos,
                    metric_u64(sample, "total_events"),
                ),
                "bytes_per_event": average_or_none(
                    total_bytes,
                    metric_u64(sample, "total_events"),
                ),
                "reference_cycles_per_event": average_or_none(
                    total_reference_cycles,
                    metric_u64(sample, "total_events"),
                ),
                "mib_s": throughput_mib_s(total_bytes, total_nanos / 1_000_000.0),
                "source": sample["_source"],
            }
        )
    rows.sort(key=lambda row: row["nanos"], reverse=True)
    return rows


def component_heap_rows(paths: list[Path]) -> list[dict]:
    rows = []
    for sample in parse_perf_metrics(paths):
        name = metric_name(sample)
        if not name.startswith("kernel;component-host-heap;"):
            continue
        total_bytes = metric_u64(sample, "total_bytes")
        total_events = metric_u64(sample, "total_events")
        rows.append(
            {
                "name": name,
                "events": total_events,
                "bytes": total_bytes,
                "bytes_per_event": average_or_none(total_bytes, total_events),
                "source": sample["_source"],
            }
        )
    rows.sort(key=lambda row: (row["bytes"], row["events"]), reverse=True)
    return rows


def average_or_none(total: int, count: int) -> float | None:
    if count == 0:
        return None
    return total / count


def artifact_provenance(manifest: dict, workloads: list[dict]) -> list[dict]:
    boot_path = repo_root() / "tools/wasi-apps/boot-artifacts.toml"
    with boot_path.open("rb") as handle:
        boot = tomllib.load(handle)
    needed = {"dash"}
    for workload in workloads:
        needed.update(workload.get("boot_programs", []))
        if workload["runner"] == "helios-aot":
            needed.add("curl")
    artifacts = []
    for artifact in boot.get("artifact", []):
        if artifact["command"] not in needed:
            continue
        artifacts.append(
            {
                "command": artifact["command"],
                "package": artifact["package"],
                "version": artifact["version"],
                "source": artifact["source_url"],
                "wasm": artifact["source"],
            }
        )
    return artifacts


def wasm_uses_simd(path: Path) -> bool:
    try:
        return fedora_wasm_uses_simd(repo_root(), path)
    except subprocess.CalledProcessError as error:
        raise SystemExit(f"failed to inspect wasm artifact for SIMD: {path}") from error


def wasm_simd_provenance(manifest: dict, workloads: list[dict]) -> list[dict]:
    provenance = []
    for artifact in artifact_provenance(manifest, workloads):
        wasm_path = repo_root() / artifact["wasm"]
        provenance.append(
            {
                "command": artifact["command"],
                "wasm": artifact["wasm"],
                "uses_simd": wasm_uses_simd(wasm_path),
            }
        )
    return provenance


def quickjs_simd_fairness(linux_provenance: dict | None) -> dict | None:
    if not linux_provenance or "quickjs_native_policy_id" not in linux_provenance:
        return None
    return {
        "wasm_path": linux_provenance["quickjs_wasm_path"],
        "wasm_uses_simd": linux_provenance["quickjs_wasm_uses_simd"],
        "native_policy_id": linux_provenance["quickjs_native_policy_id"],
        "native_c_flags_release": linux_provenance["quickjs_native_c_flags_release"],
        "native_simd_policy": linux_provenance["quickjs_native_simd_policy"],
        "baseline_strategy": linux_provenance["quickjs_baseline_strategy"],
    }


def ratio(helios_ms: int | None, linux_seconds: float | None) -> str:
    if helios_ms is None or linux_seconds is None:
        return "n/a"
    linux_ms = linux_seconds * 1000.0
    if linux_ms == 0:
        return "n/a"
    return f"{helios_ms / linux_ms:.2f}x"


def comparison_rows(
    workloads: list[dict],
    helios: dict[str, dict],
    linux: dict[str, dict],
    wasmtime_linux: dict[str, dict],
) -> list[dict]:
    rows = []
    for workload in workloads:
        helios_summary = helios.get(workload["name"])
        linux_summary = linux.get(workload["name"])
        wasmtime_summary = wasmtime_linux.get(workload["name"])
        helios_ms = helios_summary.get("median_elapsed_ms") if helios_summary else None
        linux_seconds = linux_summary.get("median") if linux_summary else None
        wasmtime_seconds = wasmtime_summary.get("median") if wasmtime_summary else None
        throughput_bytes = workload.get("throughput_bytes")
        rows.append(
            {
                "name": workload["name"],
                "class": workload["class"],
                "helios_ms": helios_ms,
                "linux_ms": linux_seconds * 1000.0 if linux_seconds is not None else None,
                "wasmtime_linux_ms": wasmtime_seconds * 1000.0
                if wasmtime_seconds is not None
                else None,
                "throughput_bytes": throughput_bytes,
            }
        )
    return rows


def comparison_summary(
    workloads: list[dict],
    helios: dict[str, dict],
    linux: dict[str, dict],
    wasmtime_linux: dict[str, dict],
) -> list[dict]:
    summary = []
    for workload in workloads:
        helios_summary = helios.get(workload["name"])
        linux_summary = linux.get(workload["name"])
        wasmtime_summary = wasmtime_linux.get(workload["name"])
        helios_ms = helios_summary.get("median_elapsed_ms") if helios_summary else None
        linux_seconds = linux_summary.get("median") if linux_summary else None
        linux_ms = linux_seconds * 1000.0 if linux_seconds is not None else None
        wasmtime_seconds = wasmtime_summary.get("median") if wasmtime_summary else None
        wasmtime_ms = wasmtime_seconds * 1000.0 if wasmtime_seconds is not None else None
        byte_count = workload.get("throughput_bytes")
        ratio_value = helios_ms / linux_ms if helios_ms is not None and linux_ms else None
        wasmtime_ratio_value = (
            helios_ms / wasmtime_ms
            if helios_ms is not None and wasmtime_ms
            else None
        )
        summary.append(
            {
                "name": workload["name"],
                "class": workload["class"],
                "helios_median_ms": helios_ms,
                "linux_median_ms": linux_ms,
                "wasmtime_linux_median_ms": wasmtime_ms,
                "helios_to_linux_ratio": ratio_value,
                "helios_to_wasmtime_linux_ratio": wasmtime_ratio_value,
                "helios_beats_wasmtime_linux": (
                    helios_ms < wasmtime_ms
                    if helios_ms is not None and wasmtime_ms is not None
                    else None
                ),
                "throughput_bytes": byte_count,
                "helios_mib_per_second": throughput_mib_s(byte_count, helios_ms),
                "linux_mib_per_second": throughput_mib_s(byte_count, linux_ms),
                "wasmtime_linux_mib_per_second": throughput_mib_s(byte_count, wasmtime_ms),
                "helios_validation_ok": bool(
                    helios_summary and helios_summary["validation"]["ok"]
                ),
            }
        )
    return summary


def write_summary_json(
    path: Path,
    manifest: dict,
    workloads: list[dict],
    run_record: dict,
    helios: dict[str, dict],
    linux: dict[str, dict],
    wasmtime_linux: dict[str, dict],
    linux_provenance: dict | None,
    host_load: dict,
    network_perf: list[dict],
    component_heap: list[dict],
    helios_kernel_flamegraphs: list[Path],
    helios_kernel_profile_top: list[dict],
) -> None:
    payload = {
        "schema_version": 1,
        "helios_git_sha": run_record.get("git_sha", git_sha()),
        "linux_baseline": linux_provenance,
        "wasmtime_linux_baseline": {
            "kind": (
                f"wasmtime-on-{linux_provenance['kind']}" if linux_provenance else "not-run"
            ),
            "release": (linux_provenance or {}).get("wasmtime_linux_release"),
            "workloads": [
                workload["name"]
                for workload in runner.workloads_with_counterpart(workloads, "linux_wasmtime")
            ],
        },
        "vm": run_record.get("vm"),
        "host": {
            "cpu": host_cpu(),
            "logical_cpus": os.cpu_count(),
            "memory": host_memory(),
            "load": host_load["load"],
            "top_cpu_processes": host_load["top_cpu_processes"],
        },
        "workloads": comparison_summary(workloads, helios, linux, wasmtime_linux),
        "network_hotspots": [
            {
                "name": row["name"],
                "events": row["events"],
                "total_bytes": row["bytes"],
                "total_nanos": row["nanos"],
                "total_reference_cycles": row["reference_cycles"],
                "nanos_per_event": row["nanos_per_event"],
                "bytes_per_event": row["bytes_per_event"],
                "reference_cycles_per_event": row["reference_cycles_per_event"],
                "mib_per_second": row["mib_s"],
                "source": str(row["source"]),
            }
            for row in network_perf[:16]
        ],
        "component_host_heap_hotspots": [
            {
                "name": row["name"],
                "events": row["events"],
                "total_bytes": row["bytes"],
                "bytes_per_event": row["bytes_per_event"],
                "source": str(row["source"]),
            }
            for row in component_heap[:16]
        ],
        "wasm_simd": wasm_simd_provenance(manifest, workloads),
        "quickjs_simd_fairness": quickjs_simd_fairness(linux_provenance),
        "helios_kernel_flamegraphs": [str(path) for path in helios_kernel_flamegraphs],
        "helios_kernel_profile_top": [
            {
                "stack": row["stack"],
                "total_nanos": row["nanos"],
                "percent": row["percent"],
                "source": str(row["source"]),
            }
            for row in helios_kernel_profile_top
        ],
    }
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def throughput_mib_s(byte_count: int | None, elapsed_ms: float | int | None) -> float | None:
    if byte_count is None or elapsed_ms is None or elapsed_ms == 0:
        return None
    return (byte_count / (1024.0 * 1024.0)) / (elapsed_ms / 1000.0)


def throughput_pair(
    workload: dict,
    helios_ms: int | None,
    linux_seconds: float | None,
    wasmtime_seconds: float | None,
) -> str:
    byte_count = workload.get("throughput_bytes")
    helios_rate = throughput_mib_s(byte_count, helios_ms)
    linux_rate = throughput_mib_s(
        byte_count,
        linux_seconds * 1000.0 if linux_seconds is not None else None,
    )
    wasmtime_rate = throughput_mib_s(
        byte_count,
        wasmtime_seconds * 1000.0 if wasmtime_seconds is not None else None,
    )
    helios_text = "n/a" if helios_rate is None else f"{helios_rate:.1f} MiB/s"
    linux_text = "n/a" if linux_rate is None else f"{linux_rate:.1f} MiB/s"
    wasmtime_text = "n/a" if wasmtime_rate is None else f"{wasmtime_rate:.1f} MiB/s"
    if byte_count is None:
        return "n/a"
    return f"H {helios_text} / L {linux_text} / W {wasmtime_text}"


def linux_baseline_label(linux_provenance: dict | None) -> str:
    """Name the guest both Linux lanes ran in, as recorded by the VM itself."""
    if not linux_provenance:
        return "the Fedora QEMU Linux guest"
    return f"the {linux_provenance['kind']} guest"


def write_svg(path: Path, rows: list[dict], baseline_label: str) -> None:
    drawable_rows = [
        row
        for row in rows
        if row["helios_ms"] is not None
        or row["linux_ms"] is not None
        or row["wasmtime_linux_ms"] is not None
    ]
    if not drawable_rows:
        return
    width = 1180
    left = 180
    right = 190
    top = 70
    row_height = 68
    bar_height = 14
    gap = 4
    max_ms = max(
        value
        for row in drawable_rows
        for value in [row["helios_ms"], row["linux_ms"], row["wasmtime_linux_ms"]]
        if value is not None
    )
    max_ms = max(max_ms, 1.0)
    height = top + row_height * len(drawable_rows) + 70
    chart_width = width - left - right
    lines = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        '<rect width="100%" height="100%" fill="#ffffff"/>',
        '<text x="32" y="34" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="22" font-weight="700" fill="#111827">Helios vs Fedora Native vs Fedora Wasmtime</text>',
        f'<text x="32" y="58" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#4b5563">Lower is better. Both baselines run inside {html.escape(baseline_label)}.</text>',
        f'<line x1="{left}" y1="{top - 14}" x2="{left + chart_width}" y2="{top - 14}" stroke="#d1d5db" stroke-width="1"/>',
        f'<text x="{left}" y="{top - 22}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" fill="#6b7280">0 ms</text>',
        f'<text x="{left + chart_width - 64}" y="{top - 22}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" fill="#6b7280">{max_ms:.1f} ms</text>',
        f'<rect x="{width - 390}" y="28" width="14" height="14" fill="#2563eb" rx="2"/>',
        f'<text x="{width - 370}" y="40" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#374151">Helios</text>',
        f'<rect x="{width - 295}" y="28" width="14" height="14" fill="#f97316" rx="2"/>',
        f'<text x="{width - 275}" y="40" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#374151">Fedora native</text>',
        f'<rect x="{width - 165}" y="28" width="14" height="14" fill="#10b981" rx="2"/>',
        f'<text x="{width - 145}" y="40" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#374151">Wasmtime</text>',
    ]
    previous_class = None
    for index, row in enumerate(drawable_rows):
        y = top + index * row_height
        if row["class"] != previous_class:
            lines.append(
                f'<text x="32" y="{y + 2}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="11" font-weight="700" fill="#6b7280">{html.escape(row["class"].upper())}</text>'
            )
            previous_class = row["class"]
        name = html.escape(row["name"])
        lines.append(
            f'<text x="32" y="{y + 26}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="14" fill="#111827">{name}</text>'
        )
        for offset, key, color in [
            (12, "helios_ms", "#2563eb"),
            (12 + bar_height + gap, "linux_ms", "#f97316"),
            (12 + (bar_height + gap) * 2, "wasmtime_linux_ms", "#10b981"),
        ]:
            value = row[key]
            label = "n/a" if value is None else f"{value:.2f} ms"
            bar_width = 0 if value is None else max(1, value / max_ms * chart_width)
            lines.append(
                f'<rect x="{left}" y="{y + offset}" width="{bar_width:.1f}" height="{bar_height}" fill="{color}" rx="3"/>'
            )
            lines.append(
                f'<text x="{left + bar_width + 8:.1f}" y="{y + offset + 12}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" fill="#374151">{label}</text>'
            )
        helios_ms = row["helios_ms"]
        linux_ms = row["linux_ms"]
        wasmtime_ms = row["wasmtime_linux_ms"]
        linux_ratio = "n/a" if helios_ms is None or linux_ms in (None, 0) else f"L {helios_ms / linux_ms:.2f}x"
        wasmtime_ratio = (
            "W n/a"
            if helios_ms is None or wasmtime_ms in (None, 0)
            else f"W {helios_ms / wasmtime_ms:.2f}x"
        )
        ratio_text = f"{linux_ratio} / {wasmtime_ratio}"
        lines.append(
            f'<text x="{width - 160}" y="{y + 38}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" font-weight="700" fill="#111827">{ratio_text}</text>'
        )
    lines.append("</svg>")
    path.write_text("\n".join(lines), encoding="utf-8")


def write_throughput_svg(path: Path, rows: list[dict]) -> bool:
    drawable_rows = []
    for row in rows:
        helios_rate = throughput_mib_s(row["throughput_bytes"], row["helios_ms"])
        linux_rate = throughput_mib_s(row["throughput_bytes"], row["linux_ms"])
        wasmtime_rate = throughput_mib_s(row["throughput_bytes"], row["wasmtime_linux_ms"])
        if helios_rate is None and linux_rate is None and wasmtime_rate is None:
            continue
        drawable_rows.append(
            {
                **row,
                "helios_rate": helios_rate,
                "linux_rate": linux_rate,
                "wasmtime_linux_rate": wasmtime_rate,
            }
        )
    if not drawable_rows:
        return False

    width = 1180
    left = 220
    right = 190
    top = 74
    row_height = 72
    bar_height = 15
    gap = 5
    max_rate = max(
        value
        for row in drawable_rows
        for value in [row["helios_rate"], row["linux_rate"], row["wasmtime_linux_rate"]]
        if value is not None
    )
    max_rate = max(max_rate, 1.0)
    height = top + row_height * len(drawable_rows) + 70
    chart_width = width - left - right
    lines = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        '<rect width="100%" height="100%" fill="#ffffff"/>',
        '<text x="32" y="34" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="22" font-weight="700" fill="#111827">Local Network Throughput</text>',
        '<text x="32" y="58" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#4b5563">Higher is better. Payloads are generated on the host and reached through QEMU user/virtio-net; no external network is used.</text>',
        f'<line x1="{left}" y1="{top - 14}" x2="{left + chart_width}" y2="{top - 14}" stroke="#d1d5db" stroke-width="1"/>',
        f'<text x="{left}" y="{top - 22}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" fill="#6b7280">0 MiB/s</text>',
        f'<text x="{left + chart_width - 86}" y="{top - 22}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" fill="#6b7280">{max_rate:.1f} MiB/s</text>',
        f'<rect x="{width - 390}" y="28" width="14" height="14" fill="#2563eb" rx="2"/>',
        f'<text x="{width - 370}" y="40" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#374151">Helios</text>',
        f'<rect x="{width - 295}" y="28" width="14" height="14" fill="#f97316" rx="2"/>',
        f'<text x="{width - 275}" y="40" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#374151">Fedora native</text>',
        f'<rect x="{width - 165}" y="28" width="14" height="14" fill="#10b981" rx="2"/>',
        f'<text x="{width - 145}" y="40" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#374151">Wasmtime</text>',
    ]
    for index, row in enumerate(drawable_rows):
        y = top + index * row_height
        name = html.escape(row["name"])
        payload_mib = row["throughput_bytes"] / (1024 * 1024)
        lines.append(
            f'<text x="32" y="{y + 28}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="14" fill="#111827">{name}</text>'
        )
        lines.append(
            f'<text x="32" y="{y + 45}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="11" fill="#6b7280">{payload_mib:.0f} MiB local payload</text>'
        )
        for offset, key, color in [
            (12, "helios_rate", "#2563eb"),
            (12 + bar_height + gap, "linux_rate", "#f97316"),
            (12 + (bar_height + gap) * 2, "wasmtime_linux_rate", "#10b981"),
        ]:
            value = row[key]
            label = "n/a" if value is None else f"{value:.1f} MiB/s"
            bar_width = 0 if value is None else max(1, value / max_rate * chart_width)
            lines.append(
                f'<rect x="{left}" y="{y + offset}" width="{bar_width:.1f}" height="{bar_height}" fill="{color}" rx="3"/>'
            )
            lines.append(
                f'<text x="{left + bar_width + 8:.1f}" y="{y + offset + 13}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" fill="#374151">{label}</text>'
            )
        helios_rate = row["helios_rate"]
        linux_rate = row["linux_rate"]
        wasmtime_rate = row["wasmtime_linux_rate"]
        linux_ratio = (
            "L n/a"
            if helios_rate is None or linux_rate in (None, 0)
            else f"L {helios_rate / linux_rate:.2f}x"
        )
        wasmtime_ratio = (
            "W n/a"
            if helios_rate is None or wasmtime_rate in (None, 0)
            else f"W {helios_rate / wasmtime_rate:.2f}x"
        )
        ratio_text = f"{linux_ratio} / {wasmtime_ratio}"
        lines.append(
            f'<text x="{width - 160}" y="{y + 40}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" font-weight="700" fill="#111827">{ratio_text}</text>'
        )
    lines.append("</svg>")
    path.write_text("\n".join(lines), encoding="utf-8")
    return True


def write_report(
    path: Path,
    manifest: dict,
    workloads: list[dict],
    helios_jsonl: Path | None,
    linux_json: Path | None,
    wasmtime_linux_json: Path | None,
    linux_provenance: dict | None,
    host_load: dict,
    wasmtime_profiles: list[Path],
) -> None:
    run_record, helios = parse_jsonl(helios_jsonl)
    linux = parse_linux_jsonl(linux_json)
    wasmtime_linux = parse_linux_jsonl(wasmtime_linux_json)
    rows = comparison_rows(workloads, helios, linux, wasmtime_linux)
    perf_metric_paths = helios_perf_metric_paths(helios_jsonl)
    helios_kernel_flamegraphs = render_helios_kernel_flamegraphs(helios_jsonl)
    helios_kernel_profile_top = helios_kernel_profile_top_rows(helios_jsonl)
    baseline_label = linux_baseline_label(linux_provenance)
    svg_path = path.with_name("helios-vs-linux.svg")
    write_svg(svg_path, rows, baseline_label)
    throughput_svg_path = path.with_name("network-throughput.svg")
    has_throughput_svg = write_throughput_svg(throughput_svg_path, rows)
    network_perf = network_perf_rows(perf_metric_paths)
    component_heap = component_heap_rows(perf_metric_paths)
    summary_json = path.with_name("summary.json")
    write_summary_json(
        summary_json,
        manifest,
        workloads,
        run_record,
        helios,
        linux,
        wasmtime_linux,
        linux_provenance,
        host_load,
        network_perf,
        component_heap,
        helios_kernel_flamegraphs,
        helios_kernel_profile_top,
    )
    lines = [
        "# Helios vs Fedora QEMU Linux and Wasmtime Benchmark",
        "",
        "![Helios vs Fedora native and Wasmtime median timings](helios-vs-linux.svg)",
        "",
        f"- Helios JSONL: `{helios_jsonl or 'not-run'}`",
        f"- Linux native JSONL: `{linux_json or 'not-run'}`",
        f"- Wasmtime-on-Linux JSONL: `{wasmtime_linux_json or 'not-run'}`",
        f"- Machine-readable summary: `{summary_json}`",
        f"- Helios git SHA: `{run_record.get('git_sha', git_sha())}`",
        f"- Linux baseline: `{(linux_provenance or {}).get('kind', 'not-run')}`",
        f"- Wasmtime-on-Linux: `{(linux_provenance or {}).get('wasmtime_linux', 'not-run')}`",
        f"- Host CPU: `{host_cpu()}`",
        f"- Host logical CPUs: `{os.cpu_count()}`",
        f"- Host memory: `{host_memory()}`",
        f"- Host load average: `1m={format_load(host_load['load']['one_minute'])}, 5m={format_load(host_load['load']['five_minutes'])}, 15m={format_load(host_load['load']['fifteen_minutes'])}`",
        f"- Host 1m load per logical CPU: `{format_load(host_load['load']['one_minute_per_cpu'])}`",
        "",
    ]
    if linux_provenance:
        lines.extend(["## Linux VM", ""])
        for key, value in linux_provenance.items():
            lines.append(f"- {key.replace('_', ' ').title()}: `{value}`")
        lines.append("")
    wasmtime_floor_failures = []
    wasmtime_floor_missing = []
    for workload in workloads:
        helios_summary = helios.get(workload["name"])
        wasmtime_summary = wasmtime_linux.get(workload["name"])
        if runner.counterpart(workload, "linux_wasmtime") is None:
            wasmtime_floor_missing.append(workload["name"])
            continue
        helios_ms = helios_summary.get("median_elapsed_ms") if helios_summary else None
        wasmtime_seconds = wasmtime_summary.get("median") if wasmtime_summary else None
        wasmtime_ms = wasmtime_seconds * 1000.0 if wasmtime_seconds is not None else None
        if helios_ms is None or wasmtime_ms is None or helios_ms >= wasmtime_ms:
            wasmtime_floor_failures.append((workload["name"], helios_ms, wasmtime_ms))
    if wasmtime_linux or wasmtime_floor_failures or wasmtime_floor_missing:
        lines.extend(
            [
                "## Wasmtime-On-Linux Floor",
                "",
                f"Target: Helios must be faster than Wasmtime running the same wasm artifact inside {baseline_label}. Native Linux remains the aspirational CPU baseline and the IO target to beat.",
                "",
            ]
        )
        if wasmtime_floor_failures:
            lines.extend(["| Workload | Helios median | Wasmtime median | Floor |", "| --- | ---: | ---: | --- |"])
            for name, helios_ms, wasmtime_ms in wasmtime_floor_failures:
                helios_text = "n/a" if helios_ms is None else f"{helios_ms} ms"
                wasmtime_text = "n/a" if wasmtime_ms is None else f"{wasmtime_ms:.2f} ms"
                lines.append(f"| `{name}` | {helios_text} | {wasmtime_text} | fail |")
            lines.append("")
        else:
            lines.extend(["- All workloads with a Wasmtime-on-Linux baseline pass the floor.", ""])
        if wasmtime_floor_missing:
            missing = ", ".join(f"`{name}`" for name in wasmtime_floor_missing)
            lines.extend(
                [
                    f"- No Wasmtime-on-Linux baseline is defined for: {missing}.",
                    "",
                ]
            )
    quickjs_fairness = quickjs_simd_fairness(linux_provenance)
    if quickjs_fairness:
        lines.extend(
            [
                "## QuickJS SIMD Fairness",
                "",
                f"- Helios QuickJS wasm: `{quickjs_fairness['wasm_path']}`",
                f"- Helios QuickJS uses wasm SIMD: `{str(quickjs_fairness['wasm_uses_simd']).lower()}`",
                f"- Fedora native policy: `{quickjs_fairness['native_policy_id']}`",
                f"- Fedora release C flags: `{quickjs_fairness['native_c_flags_release']}`",
                f"- Decision: `{quickjs_fairness['native_simd_policy']}`",
                f"- Strategy: `{quickjs_fairness['baseline_strategy']}`",
                "",
            ]
        )
    simd_rows = wasm_simd_provenance(manifest, workloads)
    if simd_rows:
        lines.extend(
            [
                "## WASM SIMD Provenance",
                "",
                "| Command | WASM | Uses SIMD |",
                "| --- | --- | ---: |",
            ]
        )
        for row in simd_rows:
            simd_text = "yes" if row["uses_simd"] else "no"
            lines.append(f"| `{row['command']}` | `{row['wasm']}` | {simd_text} |")
        lines.append("")
    if host_load["top_cpu_processes"]:
        lines.extend(["## Host Load Provenance", ""])
        for process in host_load["top_cpu_processes"]:
            command = process["command"].replace("`", "'")
            lines.append(f"- PID `{process['pid']}` CPU `{process['pcpu']:.1f}%`: `{command}`")
        lines.append("")
    vm = run_record.get("vm")
    if vm:
        lines.extend(
            [
                "## Helios VM",
                "",
                f"- Arch: `{vm['arch']}`",
                f"- Release: `{str(vm['release']).lower()}`",
                f"- QEMU accel: `{', '.join(vm['accel']) or 'none'}`",
                f"- CPU: `{vm.get('cpu') or 'default'}`",
                f"- SMP: `{vm['smp']}`",
                f"- Memory: `{vm['memory']}`",
                "",
        ]
    )
    if has_throughput_svg:
        lines.extend(
            [
                "",
                "## Network Throughput",
                "",
                "![Local network throughput](network-throughput.svg)",
                "",
            ]
        )
    if network_perf:
        lines.extend(
            [
                "## Helios Network Perf Metrics",
                "",
                "Read this table as kernel-path evidence, not as an HTTP or curl verdict: low payload bytes with high `ns/event` points at ACK/control submission cost, while high wall time with low reference cycles points at waiting for device or scheduler progress rather than CPU burn.",
                "",
                "| Metric | Events | Total bytes | Total time | Ref cycles | ns/event | ref/event | bytes/event | Throughput | Source |",
                "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
            ]
        )
        for row in network_perf[:16]:
            throughput = "n/a" if row["mib_s"] is None else f"{row['mib_s']:.1f} MiB/s"
            nanos_per_event = (
                "n/a" if row["nanos_per_event"] is None else f"{row['nanos_per_event']:.1f}"
            )
            reference_cycles_per_event = (
                "n/a"
                if row["reference_cycles_per_event"] is None
                else f"{row['reference_cycles_per_event']:.1f}"
            )
            bytes_per_event = (
                "n/a" if row["bytes_per_event"] is None else f"{row['bytes_per_event']:.1f}"
            )
            lines.append(
                f"| `{row['name']}` | {row['events']} | {row['bytes']} | {row['nanos']} ns | {row['reference_cycles']} | {nanos_per_event} | {reference_cycles_per_event} | {bytes_per_event} | {throughput} | `{row['source']}` |"
            )
        lines.append("")
    if component_heap:
        lines.extend(
            [
                "## Component Host Heap Hotspots",
                "",
                "These rows are phase-local heap deltas captured while kernel profiling is enabled. They are inclusive for outer phases, so use the most specific matching phase when chasing allocation sources.",
                "",
                "| Metric | Events | Total bytes | bytes/event | Source |",
                "| --- | ---: | ---: | ---: | --- |",
            ]
        )
        for row in component_heap[:16]:
            bytes_per_event = (
                "n/a" if row["bytes_per_event"] is None else f"{row['bytes_per_event']:.1f}"
            )
            lines.append(
                f"| `{row['name']}` | {row['events']} | {row['bytes']} | {bytes_per_event} | `{row['source']}` |"
            )
        lines.append("")
    if helios_kernel_flamegraphs:
        lines.extend(["## Helios Kernel Flamegraphs", ""])
        for flamegraph_path in helios_kernel_flamegraphs:
            lines.append(f"![{flamegraph_path.name}]({flamegraph_path.name})")
            lines.append("")
    if helios_kernel_profile_top:
        lines.extend(
            [
                "## Helios Kernel Profile Top Stacks",
                "",
                "| Stack | Total time | Share | Source |",
                "| --- | ---: | ---: | --- |",
            ]
        )
        for row in helios_kernel_profile_top:
            lines.append(
                f"| `{row['stack']}` | {row['nanos']} ns | {row['percent']:.2f}% | `{row['source']}` |"
            )
        lines.append("")

    for workload_class in WORKLOAD_CLASSES:
        title = workload_class.capitalize()
        class_workloads = [workload for workload in workloads if workload["class"] == workload_class]
        if not class_workloads:
            continue
        lines.extend(
            [
                f"## {title}",
                "",
                "| Workload | Helios median | Linux median | Wasmtime median | H/Linux | H/Wasmtime | Throughput | Validation |",
                "| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
            ]
        )
        for workload in class_workloads:
            helios_summary = helios.get(workload["name"])
            linux_summary = linux.get(workload["name"])
            wasmtime_summary = wasmtime_linux.get(workload["name"])
            helios_ms = helios_summary.get("median_elapsed_ms") if helios_summary else None
            linux_seconds = linux_summary.get("median") if linux_summary else None
            wasmtime_seconds = wasmtime_summary.get("median") if wasmtime_summary else None
            helios_text = f"{helios_ms} ms" if helios_ms is not None else "n/a"
            linux_text = f"{linux_seconds * 1000.0:.2f} ms" if linux_seconds is not None else "n/a"
            wasmtime_text = (
                f"{wasmtime_seconds * 1000.0:.2f} ms"
                if wasmtime_seconds is not None
                else "n/a"
            )
            validation = "ok" if helios_summary and helios_summary["validation"]["ok"] else "missing"
            if workload["runner"] == "helios-aot":
                validation = "helios-aot"
            lines.append(
                f"| `{workload['name']}` | {helios_text} | {linux_text} | {wasmtime_text} | {ratio(helios_ms, linux_seconds)} | {ratio(helios_ms, wasmtime_seconds)} | {throughput_pair(workload, helios_ms, linux_seconds, wasmtime_seconds)} | {validation} |"
            )
        lines.append("")

    lines.extend(["## Artifact Provenance", ""])
    for artifact in artifact_provenance(manifest, workloads):
        lines.append(
            f"- `{artifact['command']}`: `{artifact['package']}@{artifact['version']}`, source `{artifact['source']}`"
        )
    lines.extend(
        [
            "",
            "## Raw Iterations",
            "",
            f"- Helios raw iteration timings are in `{helios_jsonl or 'not-run'}`.",
            f"- Linux native raw iteration timings are in `{linux_json or 'not-run'}`.",
            f"- Wasmtime-on-Linux raw iteration timings are in `{wasmtime_linux_json or 'not-run'}`.",
        ]
    )
    for perf_path in perf_metric_paths:
        lines.append(f"- Helios perf metrics are in `{perf_path}`.")
    if helios_jsonl:
        for profile_path in sorted(helios_jsonl.parent.glob("helios*.kernel.folded")):
            lines.append(f"- Helios kernel folded profile is in `{profile_path}`.")
        for flamegraph_path in helios_kernel_flamegraphs:
            lines.append(f"- Helios kernel flamegraph SVG is in `{flamegraph_path}`.")
    if wasmtime_profiles:
        lines.extend(["", "## Wasmtime Native Profiling Artifacts", ""])
        for profile_path in wasmtime_profiles:
            profile = json.loads(profile_path.read_text(encoding="utf-8"))
            profile_kind = profile.get("profile_kind", "unknown")
            profile_scope = profile.get("profile_scope", "unknown")
            sample_source = profile.get("sample_source", "unknown")
            lines.append(
                f"- `{profile['workload']}` `{profile['mode']}` `{profile_kind}` `{profile_scope}` `{sample_source}` metadata: `{profile_path}`"
            )
            if "description" in profile:
                lines.append(f"- `{profile['workload']}` profiler meaning: {profile['description']}")
            if "firefox_profile_json" in profile:
                lines.append(f"- `{profile['workload']}` Firefox profiler JSON: `{profile['firefox_profile_json']}`")
            if "perf_data" in profile:
                lines.append(f"- `{profile['workload']}` perf data: `{profile['perf_data']}`")
            if "perf_jit_data" in profile:
                lines.append(f"- `{profile['workload']}` jit-injected perf data: `{profile['perf_jit_data']}`")
            if "perf_script" in profile:
                lines.append(f"- `{profile['workload']}` perf script: `{profile['perf_script']}`")
            if "svg" in profile:
                lines.append(f"- `{profile['workload']}` flamegraph SVG: `{profile['svg']}`")
            if "folded" in profile:
                lines.append(f"- `{profile['workload']}` folded perf stacks: `{profile['folded']}`")
    lines.append("")
    path.write_text("\n".join(lines), encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=repo_root() / "tools/wasi-apps/workloads.json")
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--class", dest="classes", action="append", choices=WORKLOAD_CLASSES, default=[])
    parser.add_argument("--workload", dest="workloads", action="append", default=[])
    parser.add_argument(
        "--skip-workload",
        dest="skip_workloads",
        action="append",
        default=[],
        help="leave a workload out of the run entirely, by name",
    )
    parser.add_argument("--arch", default="aarch64")
    parser.add_argument(
        "--helios-accel",
        default=None,
        help="accelerator the Helios guest boots on; the inspector requires one to be named",
    )
    parser.add_argument("--helios-host-http-host", default="10.0.2.2")
    parser.add_argument("--helios-host-tcp-host", default="10.0.2.2")
    parser.add_argument("--fedora-image-url", default=None)
    parser.add_argument(
        "--fedora-image-sha256",
        default=None,
        help="SHA256 of --fedora-image-url. Required with it; the pinned per-architecture images carry their own.",
    )
    parser.add_argument(
        "--linux-guest-arch",
        choices=sorted(set(LINUX_GUEST_ARCHES.values())),
        default=None,
        help=(
            "Fedora guest architecture for the Linux lane. Defaults to this host's "
            "architecture, the only guest that boots at a usable speed here."
        ),
    )
    parser.add_argument("--linux-vm-dir", type=Path, default=None)
    parser.add_argument(
        "--native-bin-dir",
        type=Path,
        default=None,
        help="Static Linux binaries from tools/bench/native/build.sh; defaults to artifacts/bench-native/<guest-arch>.",
    )
    parser.add_argument("--linux-vm-qemu-bin", default=None)
    parser.add_argument(
        "--linux-vm-accel",
        default=None,
        help="QEMU accelerator for the Fedora guest (default: hvf/kvm when native, else tcg).",
    )
    parser.add_argument("--linux-vm-ssh-port", type=int)
    parser.add_argument("--linux-vm-memory", default=DEFAULT_MEMORY)
    parser.add_argument("--linux-vm-smp", type=int, default=DEFAULT_SMP)
    parser.add_argument("--linux-vm-disk-size", default=DEFAULT_DISK_SIZE)
    parser.add_argument("--linux-vm-setup-timeout-seconds", type=int, default=900)
    parser.add_argument(
        "--quickjs-source-archive",
        type=Path,
        help="Pre-staged QuickJS-NG source archive used to build the Fedora native qjs baseline without guest internet access.",
    )
    parser.add_argument(
        "--wasmtime-linux-bin",
        type=Path,
        help="Pre-staged Linux wasmtime executable for the guest architecture, copied into the Fedora guest for the Wasmtime-on-Linux timing baseline. Without it the pinned Wasmtime release for the guest architecture is staged into the VM asset directory.",
    )
    parser.add_argument(
        "--wasmtime-linux-archive",
        type=Path,
        help="Pre-staged Linux Wasmtime tar archive for the guest architecture, copied into the Fedora guest for the Wasmtime-on-Linux timing baseline.",
    )
    parser.add_argument("--out-dir", type=Path)
    parser.add_argument(
        "--control",
        action="store_true",
        help="Run the manifest's control_workload before and after the suite on every side to measure machine noise.",
    )
    parser.add_argument(
        "--keep-going",
        action="store_true",
        help="record a workload that fails and continue with the next one, on every side",
    )
    parser.add_argument("--skip-helios", action="store_true")
    parser.add_argument("--skip-linux", action="store_true")
    parser.add_argument("--wasmtime-profile-workload", action="append", default=[])
    parser.add_argument(
        "--wasmtime-profile-mode",
        choices=["guest", "perfmap", "jitdump"],
        default="jitdump",
        help="Wasmtime profiling mode for selected native runs. Default jitdump uses Linux perf and emits flamegraph artifacts.",
    )
    parser.add_argument("--wasmtime-bin", default=os.environ.get("WASMTIME_BIN", "wasmtime"))
    parser.add_argument("--wasmtime-no-flamegraph", action="store_true")
    parser.add_argument("--wasmtime-profile-guest-interval", default="1ms")
    parser.add_argument("--wasmtime-profile-perf-event", default="cycles:u")
    parser.add_argument("--max-host-load-per-cpu", type=float, default=DEFAULT_MAX_HOST_LOAD_PER_CPU)
    parser.add_argument("--allow-busy-host", action="store_true")
    parser.add_argument(
        "--helios-timeout-seconds",
        type=int,
        default=DEFAULT_HELIOS_TIMEOUT_SECONDS,
        help="Maximum wall-clock seconds for each isolated Helios VM workload command.",
    )
    args = parser.parse_args()

    if args.iterations <= 0:
        raise SystemExit("--iterations must be a positive integer")
    if args.max_host_load_per_cpu <= 0:
        raise SystemExit("--max-host-load-per-cpu must be positive")
    if args.helios_timeout_seconds <= 0:
        raise SystemExit("--helios-timeout-seconds must be positive")
    if args.linux_vm_smp <= 0:
        raise SystemExit("--linux-vm-smp must be positive")
    if args.linux_vm_setup_timeout_seconds <= 0:
        raise SystemExit("--linux-vm-setup-timeout-seconds must be positive")
    if args.wasmtime_linux_bin is not None and args.wasmtime_linux_archive is not None:
        raise SystemExit("pass either --wasmtime-linux-bin or --wasmtime-linux-archive, not both")

    linux_guest_arch = args.linux_guest_arch
    if not args.skip_linux:
        if linux_guest_arch is None:
            linux_guest_arch = host_arch()
        if not args.skip_helios:
            helios_guest_arch = LINUX_GUEST_ARCHES.get(args.arch)
            if helios_guest_arch is None:
                raise SystemExit(
                    f"no Fedora baseline mapping for --arch {args.arch}; pass --skip-linux"
                )
            # A Fedora guest of a different architecture than the host runs
            # under cross-architecture TCG, where the cloud image's own device
            # and service timeouts expire before it finishes booting. Split the
            # two lanes across hosts instead of producing a cross-ISA gap.
            if helios_guest_arch != linux_guest_arch:
                raise SystemExit(
                    f"Helios --arch {args.arch} needs a {helios_guest_arch} Linux baseline, but "
                    f"this {platform.machine()} host can only run a {linux_guest_arch} Fedora "
                    "guest. Run each lane on its own hardware with --skip-linux and "
                    "--skip-helios, or force the guest with --linux-guest-arch."
                )
        if args.fedora_image_url is None:
            args.fedora_image_url = FEDORA_IMAGE_URLS[linux_guest_arch]
            args.fedora_image_sha256 = FEDORA_IMAGE_SHA256[linux_guest_arch]
        elif args.fedora_image_sha256 is None:
            raise SystemExit("--fedora-image-url requires --fedora-image-sha256")
        if args.linux_vm_qemu_bin is None:
            args.linux_vm_qemu_bin = QEMU_BINS[linux_guest_arch]
        if args.linux_vm_dir is None:
            args.linux_vm_dir = default_asset_dir(linux_guest_arch)
        if args.native_bin_dir is None:
            args.native_bin_dir = repo_root() / "artifacts/bench-native" / linux_guest_arch

    if not args.skip_helios:
        enforce_no_stale_helios_benchmark_processes()
    host_load = host_load_snapshot()
    enforce_host_load(host_load, args.max_host_load_per_cpu, args.allow_busy_host)
    manifest = load_manifest(args.manifest)
    workloads = selected_workloads(manifest, args.classes, args.workloads, args.skip_workloads)
    control_workload = None
    if args.control:
        control_workload = runner.selected_workload(manifest, manifest["control_workload"])
    out_dir = args.out_dir or repo_root() / "target/perf-baselines" / f"linux-gap-{git_short_sha()}-{int(time.time())}"
    if not out_dir.is_absolute():
        out_dir = repo_root() / out_dir
    out_dir = out_dir.resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    http_root = out_dir / "http-root"
    http_root.mkdir(exist_ok=True)
    write_http_payloads(http_root)

    profile_workloads = selected_workloads(manifest, [], args.wasmtime_profile_workload) if args.wasmtime_profile_workload else []
    needs_http = any(workload.get("requires_host_http", False) for workload in workloads + profile_workloads)
    needs_tcp = any(workload.get("requires_host_tcp", False) for workload in workloads)
    needs_tcp_echo = any(workload.get("requires_host_tcp_echo", False) for workload in workloads)
    server = None
    tcp_server = None
    tcp_echo_server = None
    host_http_url = None
    local_http_url = None
    host_tcp_host = None
    host_tcp_port = None
    host_tcp_echo_port = None
    if needs_http:
        server, port = start_host_http(http_root)
        host_http_url = f"http://{args.helios_host_http_host}:{port}/{HTTP_PAYLOAD_FILE}"
        local_http_url = f"http://127.0.0.1:{port}/{HTTP_PAYLOAD_FILE}"
    if needs_tcp and (not args.skip_helios or not args.skip_linux):
        tcp_server, port = start_tcp_throughput_server(
            HOST_SERVER_BIND_ADDRESS, 0, HTTP_LARGE_PAYLOAD_BYTES
        )
        host_tcp_host = args.helios_host_tcp_host
        host_tcp_port = port
    if needs_tcp_echo and (not args.skip_helios or not args.skip_linux):
        tcp_echo_server, port = start_tcp_echo_server("127.0.0.1", 0)
        host_tcp_host = args.helios_host_tcp_host
        host_tcp_echo_port = port
    linux_tcp_port = host_tcp_port if needs_tcp and not args.skip_linux else None
    linux_tcp_echo_port = host_tcp_echo_port if needs_tcp_echo and not args.skip_linux else None

    try:
        helios_jsonl = None
        linux_json = None
        wasmtime_linux_json = None
        linux_provenance = None
        wasmtime_profiles = []
        if not args.skip_helios:
            helios_jsonl = run_helios(
                args.manifest,
                out_dir,
                args.iterations,
                workloads,
                args.arch,
                args.helios_accel,
                host_http_url,
                host_tcp_host,
                host_tcp_port,
                host_tcp_echo_port,
                args.helios_timeout_seconds,
                control_workload,
                args.keep_going,
            )
        if not args.skip_linux:
            linux_json, wasmtime_linux_json, linux_provenance = run_linux(
                args.manifest,
                out_dir,
                args.iterations,
                workloads,
                args.fedora_image_url,
                args.fedora_image_sha256,
                args.linux_vm_dir,
                args.linux_vm_qemu_bin,
                args.linux_vm_ssh_port,
                args.linux_vm_memory,
                args.linux_vm_smp,
                args.linux_vm_disk_size,
                args.linux_vm_setup_timeout_seconds,
                host_http_url,
                host_tcp_host,
                linux_tcp_port,
                linux_tcp_echo_port,
                args.quickjs_source_archive,
                args.wasmtime_linux_bin,
                args.wasmtime_linux_archive,
                linux_guest_arch,
                args.linux_vm_accel,
                args.native_bin_dir,
                control_workload,
                args.keep_going,
            )
        if args.wasmtime_profile_workload:
            wasmtime_profiles = run_wasmtime_profiles(
                args.manifest,
                out_dir,
                args.wasmtime_profile_workload,
                args.wasmtime_profile_mode,
                local_http_url,
                args.wasmtime_bin,
                args.wasmtime_no_flamegraph,
                args.wasmtime_profile_guest_interval,
                args.wasmtime_profile_perf_event,
            )
        report = out_dir / "report.md"
        write_report(
            report,
            manifest,
            workloads,
            helios_jsonl,
            linux_json,
            wasmtime_linux_json,
            linux_provenance,
            host_load,
            wasmtime_profiles,
        )
        print(report)
    finally:
        if server:
            server.shutdown()
            server.server_close()
        if tcp_server:
            tcp_server.shutdown()
            tcp_server.server_close()
        if tcp_echo_server:
            tcp_echo_server.shutdown()
            tcp_echo_server.server_close()


if __name__ == "__main__":
    try:
        main()
    except HeliosRunFailed as error:
        # A run that gave up says so in one line; the traceback of a
        # subprocess exit code told the reader nothing.
        raise SystemExit(str(error)) from error
