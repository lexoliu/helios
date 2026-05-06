#!/usr/bin/env python3
import argparse
import html
import http.server
import json
import os
import platform
import shlex
import socketserver
import subprocess
import sys
import threading
import time
import tomllib
from pathlib import Path


DEFAULT_IMAGE = "debian:trixie"
HTTP_PAYLOAD_FILE = "payload.txt"
HTTP_LARGE_PAYLOAD_FILE = "payload-64m.bin"
HTTP_PAYLOAD = b"helios-linux-gap:ok\n"
HTTP_LARGE_PAYLOAD_BYTES = 64 * 1024 * 1024
HTTP_LARGE_PAYLOAD_CHUNK = bytes(range(251))
DEFAULT_MAX_HOST_LOAD_PER_CPU = 0.75
TOP_CPU_PROCESS_LIMIT = 8


class QuietHttpHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, format: str, *args: object) -> None:
        return


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def run(command: list[str], env: dict[str, str] | None = None) -> None:
    subprocess.run(command, cwd=repo_root(), env=env, check=True)


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
    with path.open("r", encoding="utf-8") as handle:
        manifest = json.load(handle)
    if manifest.get("schema_version") != 1:
        raise SystemExit(f"unsupported workload manifest schema_version {manifest.get('schema_version')}")
    return manifest


def selected_workloads(manifest: dict, classes: list[str], names: list[str]) -> list[dict]:
    selected = []
    for workload in manifest["workloads"]:
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
    server = socketserver.TCPServer(("127.0.0.1", 0), handler)
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


def docker_image_digest(image: str) -> str:
    try:
        raw = output(["docker", "image", "inspect", image, "--format", "{{json .RepoDigests}} {{.Id}}"])
    except subprocess.CalledProcessError:
        run(["docker", "pull", image])
        raw = output(["docker", "image", "inspect", image, "--format", "{{json .RepoDigests}} {{.Id}}"])
    digests_raw, image_id = raw.split(" ", 1)
    digests = json.loads(digests_raw)
    return digests[0] if digests else image_id


def run_helios(
    manifest: Path,
    out_dir: Path,
    iterations: int,
    workloads: list[dict],
    arch: str,
    host_http_url: str | None,
) -> Path:
    log = out_dir / "helios.jsonl"
    workloads_by_class: dict[str, list[str]] = {}
    for workload in workloads:
        workloads_by_class.setdefault(workload["class"], []).append(workload["name"])

    if len(workloads_by_class) > 1:
        class_logs = []
        for workload_class, workload_names in workloads_by_class.items():
            class_log = out_dir / f"helios-{workload_class}.jsonl"
            run_helios_once(
                manifest,
                class_log,
                iterations,
                [workload_class],
                workload_names,
                arch,
                host_http_url,
            )
            class_logs.append(class_log)
        with log.open("w", encoding="utf-8") as output_handle:
            for class_log in class_logs:
                output_handle.write(class_log.read_text(encoding="utf-8"))
        return log

    run_helios_once(
        manifest,
        log,
        iterations,
        list(workloads_by_class),
        [workload["name"] for workload in workloads],
        arch,
        host_http_url,
    )
    return log


def run_helios_once(
    manifest: Path,
    log: Path,
    iterations: int,
    classes: list[str],
    names: list[str],
    arch: str,
    host_http_url: str | None,
) -> None:
    env = os.environ.copy()
    env["HELIOS_WORKLOAD_BENCH_ARCH"] = arch
    env["HELIOS_WORKLOAD_BENCH_ITERATIONS"] = str(iterations)
    env["HELIOS_WORKLOAD_BENCH_MANIFEST"] = str(manifest)
    env["HELIOS_WORKLOAD_BENCH_LOG"] = str(log)
    env["HELIOS_WORKLOAD_BENCH_KERNEL_PROFILE_OUTPUT"] = str(log.with_suffix(".kernel.folded"))
    env["HELIOS_WORKLOAD_BENCH_PERF_METRICS_OUTPUT"] = str(log.with_suffix(".perf.json"))
    if classes:
        env["HELIOS_WORKLOAD_BENCH_CLASSES"] = ",".join(classes)
    if names:
        env["HELIOS_WORKLOAD_BENCH_WORKLOADS"] = ",".join(names)
    if host_http_url:
        env["HELIOS_WORKLOAD_BENCH_HOST_HTTP_URL"] = host_http_url
    run(["tools/wasi-apps/workload-bench.sh"], env=env)


def run_linux(
    manifest: Path,
    out_dir: Path,
    iterations: int,
    workloads: list[dict],
    image: str,
    host_http_url: str | None,
    linux_http_port: int,
) -> tuple[Path | None, str]:
    native_workloads = [
        workload for workload in workloads if workload["runner"] in ("shell", "program")
    ]
    digest = docker_image_digest(image)
    if not native_workloads:
        return None, digest

    hyperfine_json = out_dir / "linux-hyperfine.json"
    command = [
        "apt-get update",
        "DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends python3 quickjs bash dash coreutils curl hyperfine ca-certificates",
        "mkdir -p /bench",
        "cd /bench",
    ]
    if host_http_url:
        command.extend(
            [
                f"python3 -m http.server {linux_http_port} --bind 127.0.0.1 --directory /out/http-root >/tmp/helios-gap-http.log 2>&1 &",
                "server_pid=$!",
                "trap 'kill ${server_pid}' EXIT",
                "sleep 0.2",
            ]
        )

    hyperfine = [
        "hyperfine",
        "--runs",
        str(iterations),
        "--export-json",
        "/out/linux-hyperfine.json",
    ]
    linux_http_url = f"http://127.0.0.1:{linux_http_port}/{HTTP_PAYLOAD_FILE}" if host_http_url else None
    for workload in native_workloads:
        runner = [
            "python3",
            "/repo/tools/wasi-apps/linux-workload-runner.py",
            "--manifest",
            "/repo/tools/wasi-apps/workloads.json",
            "--workload",
            workload["name"],
        ]
        if linux_http_url:
            runner.extend(["--host-http-url", linux_http_url])
        hyperfine.extend(["--command-name", workload["name"], " ".join(shlex.quote(part) for part in runner)])
    command.append(" ".join(shlex.quote(part) for part in hyperfine))
    docker_script = "\n".join(command)
    run(
        [
            "docker",
            "run",
            "--rm",
            "--mount",
            f"type=bind,source={repo_root()},target=/repo,readonly",
            "--mount",
            f"type=bind,source={out_dir},target=/out",
            "-w",
            "/bench",
            image,
            "bash",
            "-lc",
            docker_script,
        ]
    )
    return hyperfine_json, digest


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


def parse_hyperfine(path: Path | None) -> dict[str, dict]:
    if path is None or not path.exists():
        return {}
    with path.open("r", encoding="utf-8") as handle:
        data = json.load(handle)
    return {entry["command"]: entry for entry in data.get("results", [])}


def helios_perf_metric_paths(helios_jsonl: Path | None) -> list[Path]:
    if helios_jsonl is None:
        return []
    return sorted(helios_jsonl.parent.glob("helios*.perf.json"))


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
        rows.append(
            {
                "name": name,
                "events": metric_u64(sample, "total_events"),
                "nanos": total_nanos,
                "bytes": total_bytes,
                "mib_s": throughput_mib_s(total_bytes, total_nanos / 1_000_000.0),
                "source": sample["_source"],
            }
        )
    rows.sort(key=lambda row: row["nanos"], reverse=True)
    return rows


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
        sha_path = repo_root() / artifact["sha256_file"]
        artifacts.append(
            {
                "command": artifact["command"],
                "package": artifact["package"],
                "version": artifact["version"],
                "source": artifact["source_url"],
                "wasm": artifact["source"],
                "sha256": sha_path.read_text(encoding="utf-8").split()[0],
            }
        )
    return artifacts


def ratio(helios_ms: int | None, linux_seconds: float | None) -> str:
    if helios_ms is None or linux_seconds is None:
        return "n/a"
    linux_ms = linux_seconds * 1000.0
    if linux_ms == 0:
        return "n/a"
    return f"{helios_ms / linux_ms:.2f}x"


def comparison_rows(workloads: list[dict], helios: dict[str, dict], linux: dict[str, dict]) -> list[dict]:
    rows = []
    for workload in workloads:
        helios_summary = helios.get(workload["name"])
        linux_summary = linux.get(workload["name"])
        helios_ms = helios_summary.get("median_elapsed_ms") if helios_summary else None
        linux_seconds = linux_summary.get("median") if linux_summary else None
        throughput_bytes = workload.get("throughput_bytes")
        rows.append(
            {
                "name": workload["name"],
                "class": workload["class"],
                "helios_ms": helios_ms,
                "linux_ms": linux_seconds * 1000.0 if linux_seconds is not None else None,
                "throughput_bytes": throughput_bytes,
            }
        )
    return rows


def throughput_mib_s(byte_count: int | None, elapsed_ms: float | int | None) -> float | None:
    if byte_count is None or elapsed_ms is None or elapsed_ms == 0:
        return None
    return (byte_count / (1024.0 * 1024.0)) / (elapsed_ms / 1000.0)


def throughput_pair(workload: dict, helios_ms: int | None, linux_seconds: float | None) -> str:
    byte_count = workload.get("throughput_bytes")
    helios_rate = throughput_mib_s(byte_count, helios_ms)
    linux_rate = throughput_mib_s(
        byte_count,
        linux_seconds * 1000.0 if linux_seconds is not None else None,
    )
    helios_text = "n/a" if helios_rate is None else f"{helios_rate:.1f} MiB/s"
    linux_text = "n/a" if linux_rate is None else f"{linux_rate:.1f} MiB/s"
    if byte_count is None:
        return "n/a"
    return f"H {helios_text} / L {linux_text}"


def write_svg(path: Path, rows: list[dict]) -> None:
    drawable_rows = [
        row for row in rows if row["helios_ms"] is not None or row["linux_ms"] is not None
    ]
    if not drawable_rows:
        return
    width = 1040
    left = 180
    right = 140
    top = 70
    row_height = 54
    bar_height = 15
    gap = 4
    max_ms = max(
        value
        for row in drawable_rows
        for value in [row["helios_ms"], row["linux_ms"]]
        if value is not None
    )
    max_ms = max(max_ms, 1.0)
    height = top + row_height * len(drawable_rows) + 70
    chart_width = width - left - right
    lines = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        '<rect width="100%" height="100%" fill="#ffffff"/>',
        '<text x="32" y="34" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="22" font-weight="700" fill="#111827">Helios vs Native Linux Median Timing</text>',
        '<text x="32" y="58" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#4b5563">Lower is better. Ratio below 1.00x means Helios is faster.</text>',
        f'<line x1="{left}" y1="{top - 14}" x2="{left + chart_width}" y2="{top - 14}" stroke="#d1d5db" stroke-width="1"/>',
        f'<text x="{left}" y="{top - 22}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" fill="#6b7280">0 ms</text>',
        f'<text x="{left + chart_width - 64}" y="{top - 22}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" fill="#6b7280">{max_ms:.1f} ms</text>',
        f'<rect x="{width - 250}" y="28" width="14" height="14" fill="#2563eb" rx="2"/>',
        f'<text x="{width - 230}" y="40" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#374151">Helios</text>',
        f'<rect x="{width - 160}" y="28" width="14" height="14" fill="#f97316" rx="2"/>',
        f'<text x="{width - 140}" y="40" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#374151">Linux</text>',
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
        ratio_text = "n/a" if helios_ms is None or linux_ms in (None, 0) else f"{helios_ms / linux_ms:.2f}x"
        lines.append(
            f'<text x="{width - 94}" y="{y + 31}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" font-weight="700" fill="#111827">{ratio_text}</text>'
        )
    lines.append("</svg>")
    path.write_text("\n".join(lines), encoding="utf-8")


def write_throughput_svg(path: Path, rows: list[dict]) -> bool:
    drawable_rows = []
    for row in rows:
        helios_rate = throughput_mib_s(row["throughput_bytes"], row["helios_ms"])
        linux_rate = throughput_mib_s(row["throughput_bytes"], row["linux_ms"])
        if helios_rate is None and linux_rate is None:
            continue
        drawable_rows.append({**row, "helios_rate": helios_rate, "linux_rate": linux_rate})
    if not drawable_rows:
        return False

    width = 1040
    left = 220
    right = 150
    top = 74
    row_height = 58
    bar_height = 16
    gap = 5
    max_rate = max(
        value
        for row in drawable_rows
        for value in [row["helios_rate"], row["linux_rate"]]
        if value is not None
    )
    max_rate = max(max_rate, 1.0)
    height = top + row_height * len(drawable_rows) + 70
    chart_width = width - left - right
    lines = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        '<rect width="100%" height="100%" fill="#ffffff"/>',
        '<text x="32" y="34" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="22" font-weight="700" fill="#111827">Local HTTP Throughput</text>',
        '<text x="32" y="58" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#4b5563">Higher is better. Payloads are generated locally; no external network is used.</text>',
        f'<line x1="{left}" y1="{top - 14}" x2="{left + chart_width}" y2="{top - 14}" stroke="#d1d5db" stroke-width="1"/>',
        f'<text x="{left}" y="{top - 22}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" fill="#6b7280">0 MiB/s</text>',
        f'<text x="{left + chart_width - 86}" y="{top - 22}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="12" fill="#6b7280">{max_rate:.1f} MiB/s</text>',
        f'<rect x="{width - 250}" y="28" width="14" height="14" fill="#2563eb" rx="2"/>',
        f'<text x="{width - 230}" y="40" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#374151">Helios</text>',
        f'<rect x="{width - 160}" y="28" width="14" height="14" fill="#f97316" rx="2"/>',
        f'<text x="{width - 140}" y="40" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" fill="#374151">Linux</text>',
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
        ratio_text = "n/a" if helios_rate is None or linux_rate in (None, 0) else f"{helios_rate / linux_rate:.2f}x"
        lines.append(
            f'<text x="{width - 98}" y="{y + 34}" font-family="ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="13" font-weight="700" fill="#111827">{ratio_text}</text>'
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
    docker_digest: str | None,
    host_load: dict,
    wasmtime_profiles: list[Path],
) -> None:
    run_record, helios = parse_jsonl(helios_jsonl)
    linux = parse_hyperfine(linux_json)
    rows = comparison_rows(workloads, helios, linux)
    perf_metric_paths = helios_perf_metric_paths(helios_jsonl)
    svg_path = path.with_name("helios-vs-linux.svg")
    write_svg(svg_path, rows)
    throughput_svg_path = path.with_name("network-throughput.svg")
    has_throughput_svg = write_throughput_svg(throughput_svg_path, rows)
    lines = [
        "# Helios vs Native Linux Benchmark",
        "",
        "![Helios vs Linux median timings](helios-vs-linux.svg)",
        "",
        f"- Helios JSONL: `{helios_jsonl or 'not-run'}`",
        f"- Linux Hyperfine JSON: `{linux_json or 'not-run'}`",
        f"- Helios git SHA: `{run_record.get('git_sha', git_sha())}`",
        f"- Docker image digest: `{docker_digest or 'not-run'}`",
        f"- Host CPU: `{host_cpu()}`",
        f"- Host logical CPUs: `{os.cpu_count()}`",
        f"- Host memory: `{host_memory()}`",
        f"- Host load average: `1m={format_load(host_load['load']['one_minute'])}, 5m={format_load(host_load['load']['five_minutes'])}, 15m={format_load(host_load['load']['fifteen_minutes'])}`",
        f"- Host 1m load per logical CPU: `{format_load(host_load['load']['one_minute_per_cpu'])}`",
        "",
    ]
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
                "![Local HTTP throughput](network-throughput.svg)",
                "",
            ]
        )
    network_perf = network_perf_rows(perf_metric_paths)
    if network_perf:
        lines.extend(
            [
                "## Helios Network Perf Metrics",
                "",
                "| Metric | Events | Total bytes | Total time | Throughput | Source |",
                "| --- | ---: | ---: | ---: | ---: | --- |",
            ]
        )
        for row in network_perf[:16]:
            throughput = "n/a" if row["mib_s"] is None else f"{row['mib_s']:.1f} MiB/s"
            lines.append(
                f"| `{row['name']}` | {row['events']} | {row['bytes']} | {row['nanos']} ns | {throughput} | `{row['source']}` |"
            )
        lines.append("")

    for workload_class, title in [("cpu-bound", "CPU-Bound"), ("io-bound", "IO-Bound")]:
        class_workloads = [workload for workload in workloads if workload["class"] == workload_class]
        if not class_workloads:
            continue
        lines.extend(
            [
                f"## {title}",
                "",
                "| Workload | Helios median | Linux median | Ratio | Throughput | Validation |",
                "| --- | ---: | ---: | ---: | ---: | --- |",
            ]
        )
        for workload in class_workloads:
            helios_summary = helios.get(workload["name"])
            linux_summary = linux.get(workload["name"])
            helios_ms = helios_summary.get("median_elapsed_ms") if helios_summary else None
            linux_seconds = linux_summary.get("median") if linux_summary else None
            helios_text = f"{helios_ms} ms" if helios_ms is not None else "n/a"
            linux_text = f"{linux_seconds * 1000.0:.2f} ms" if linux_seconds is not None else "n/a"
            validation = "ok" if helios_summary and helios_summary["validation"]["ok"] else "missing"
            if workload["runner"] == "helios-aot":
                validation = "helios-aot"
            lines.append(
                f"| `{workload['name']}` | {helios_text} | {linux_text} | {ratio(helios_ms, linux_seconds)} | {throughput_pair(workload, helios_ms, linux_seconds)} | {validation} |"
            )
        lines.append("")

    lines.extend(["## Artifact Provenance", ""])
    for artifact in artifact_provenance(manifest, workloads):
        lines.append(
            f"- `{artifact['command']}`: `{artifact['package']}@{artifact['version']}`, sha256 `{artifact['sha256']}`, source `{artifact['source']}`"
        )
    lines.extend(
        [
            "",
            "## Raw Iterations",
            "",
            f"- Helios raw iteration timings are in `{helios_jsonl or 'not-run'}`.",
            f"- Linux raw hyperfine timings are in `{linux_json or 'not-run'}`.",
        ]
    )
    for perf_path in perf_metric_paths:
        lines.append(f"- Helios perf metrics are in `{perf_path}`.")
    if helios_jsonl:
        for profile_path in sorted(helios_jsonl.parent.glob("helios*.kernel.folded")):
            lines.append(f"- Helios kernel folded profile is in `{profile_path}`.")
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
    parser.add_argument("--class", dest="classes", action="append", choices=["cpu-bound", "io-bound"], default=[])
    parser.add_argument("--workload", dest="workloads", action="append", default=[])
    parser.add_argument("--arch", default="aarch64")
    parser.add_argument("--docker-image", default=DEFAULT_IMAGE)
    parser.add_argument("--helios-host-http-host", default="10.0.2.2")
    parser.add_argument("--linux-http-port", type=int, default=18080)
    parser.add_argument("--out-dir", type=Path)
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
    args = parser.parse_args()

    if args.iterations <= 0:
        raise SystemExit("--iterations must be a positive integer")
    if args.max_host_load_per_cpu <= 0:
        raise SystemExit("--max-host-load-per-cpu must be positive")

    host_load = host_load_snapshot()
    enforce_host_load(host_load, args.max_host_load_per_cpu, args.allow_busy_host)
    manifest = load_manifest(args.manifest)
    workloads = selected_workloads(manifest, args.classes, args.workloads)
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
    server = None
    host_http_url = None
    local_http_url = None
    if needs_http:
        server, port = start_host_http(http_root)
        host_http_url = f"http://{args.helios_host_http_host}:{port}/{HTTP_PAYLOAD_FILE}"
        local_http_url = f"http://127.0.0.1:{port}/{HTTP_PAYLOAD_FILE}"

    try:
        helios_jsonl = None
        linux_json = None
        docker_digest = None
        wasmtime_profiles = []
        if not args.skip_helios:
            helios_jsonl = run_helios(
                args.manifest,
                out_dir,
                args.iterations,
                workloads,
                args.arch,
                host_http_url,
            )
        if not args.skip_linux:
            linux_json, docker_digest = run_linux(
                args.manifest,
                out_dir,
                args.iterations,
                workloads,
                args.docker_image,
                host_http_url,
                args.linux_http_port,
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
            docker_digest,
            host_load,
            wasmtime_profiles,
        )
        print(report)
    finally:
        if server:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
