#!/usr/bin/env python3
"""Runs the Linux counterparts of the benchmark workloads inside the guest.

Every workload in ``tools/wasi-apps/workloads.json`` declares a
``counterparts.linux_native`` and a ``counterparts.linux_wasmtime`` entry.
This runner resolves one of them into a command line, executes it for the
requested number of iterations, times only the child (never this
interpreter), parses the ``bench.<name>=<value>`` lines it prints, and
writes the same JSONL record shapes as the inspector's ``workload-bench``.

Subcommands:

    run         time workloads, write JSONL
    precompile  ``wasmtime compile`` every wasm the manifest runs under Wasmtime
    guest-paths print the repo-relative paths the Linux side needs copied in

The file is importable (``linux_workload_runner``) so the host-side Fedora
driver shares the manifest interpretation instead of re-implementing it.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

SCHEMA_VERSION = 2
METRIC_LINE_PREFIX = "bench."
SIDES = ("linux_native", "linux_wasmtime")

# Placeholders naming a tool the Helios manifest runs by bootfs path; the
# Linux side resolves them to the distribution's binaries.
LINUX_TOOLS = {
    "{bash}": "bash",
    "{cat}": "cat",
    "{curl}": "curl --fail --silent --show-error",
    "{dash}": "dash",
    "{head}": "head",
    "{mkdir}": "mkdir",
    "{python3}": "python3",
    "{quickjs}": "qjs",
    "{simd_lanes}": "/usr/local/bin/helios-simd-lanes",
    "{tcp_throughput}": "python3 {repo_root}/tools/wasi-apps/linux_tcp_throughput_client.py",
    "{wasi_tcp_throughput}": "python3 {repo_root}/tools/wasi-apps/linux_tcp_throughput_client.py --label wasi-tcp-throughput",
    "{wasix_tcp_throughput}": "python3 {repo_root}/tools/wasi-apps/linux_tcp_throughput_client.py --label wasix-tcp-throughput",
}

HTTP_LARGE_PAYLOAD_FILE = "payload-64m.bin"
NATIVE_BIN_DIR = "native"
WASMTIME_RUN_PLACEHOLDER = "{wasmtime_run}"
GUEST_PATH_PATTERN = re.compile(r"\{(guest|cwasm):([^}]+)\}")
REPO_ROOT_PATH_PATTERN = re.compile(r"\{repo_root\}/([^:\s]+)")


@dataclass(frozen=True)
class HostEndpoints:
    http_url: str | None = None
    tcp_host: str | None = None
    tcp_port: int | None = None
    tcp_echo_port: int | None = None


@dataclass(frozen=True)
class RenderContext:
    repo_root: Path
    wasmtime_bin: str
    hosts: HostEndpoints
    workdir: Path


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def large_http_url(host_http_url: str) -> str:
    prefix, separator, _ = host_http_url.rpartition("/")
    if not separator:
        raise SystemExit(f"host HTTP URL has no path segment: {host_http_url}")
    return f"{prefix}/{HTTP_LARGE_PAYLOAD_FILE}"


def load_manifest(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as handle:
        manifest = json.load(handle)
    if manifest.get("schema_version") != SCHEMA_VERSION:
        raise SystemExit(
            f"unsupported workload manifest schema_version {manifest.get('schema_version')}, "
            f"expected {SCHEMA_VERSION}"
        )
    return manifest


def selected_workload(manifest: dict, name: str) -> dict:
    for workload in manifest["workloads"]:
        if workload["name"] == name:
            return workload
    raise SystemExit(f"unknown workload {name}")


def counterpart(workload: dict, side: str) -> dict | None:
    """The counterpart spec for ``side`` or ``None`` when the manifest says so."""
    if side not in SIDES:
        raise SystemExit(f"unknown counterpart side {side}")
    counterparts = workload.get("counterparts")
    if counterparts is None or side not in counterparts:
        raise SystemExit(f"workload {workload['name']} does not declare counterparts.{side}")
    return counterparts[side]


def workloads_with_counterpart(workloads: list[dict], side: str) -> list[dict]:
    return [workload for workload in workloads if counterpart(workload, side) is not None]


def _shape(workload: dict, spec: dict) -> dict:
    """The command shape a counterpart resolves to: the workload's own when
    it inherits, otherwise the spec itself."""
    if spec.get("inherit"):
        if workload["runner"] not in ("shell", "program"):
            raise SystemExit(
                f"workload {workload['name']} cannot inherit a {workload['runner']} runner on Linux"
            )
        return workload
    return spec


def counterpart_templates(workload: dict, side: str) -> list[str]:
    """Every template string a counterpart renders, for placeholder scans."""
    spec = counterpart(workload, side)
    if spec is None:
        return []
    shape = _shape(workload, spec)
    templates = [shape.get("command", ""), shape.get("program", ""), *shape.get("args", [])]
    templates.extend(spec.get("wasmtime_flags", []))
    return [template for template in templates if template]


def uses_placeholder(workload: dict, side: str, placeholder: str) -> bool:
    return any(placeholder in template for template in counterpart_templates(workload, side))


def guest_paths(root: Path, workloads: list[dict]) -> list[Path]:
    """Repo-relative files the Linux guest needs for its counterparts."""
    paths: dict[str, Path] = {}
    for workload in workloads:
        for side in SIDES:
            spec = counterpart(workload, side)
            if spec is None:
                continue
            relative = [*spec.get("guest_paths", [])]
            if "wasm_path" in spec:
                relative.append(spec["wasm_path"])
            for template in counterpart_templates(workload, side):
                relative.extend(match.group(2) for match in GUEST_PATH_PATTERN.finditer(template))
                relative.extend(match.group(1) for match in REPO_ROOT_PATH_PATTERN.finditer(template))
            for entry in relative:
                path = root / entry
                if not path.exists():
                    raise SystemExit(
                        f"workload {workload['name']} {side} needs a missing artifact: {path}"
                    )
                paths[entry] = path
    return [paths[key] for key in sorted(paths)]


def precompile_sources(root: Path, workloads: list[dict]) -> list[Path]:
    """Every wasm the Linux+Wasmtime side runs, to be compiled once up front."""
    sources: dict[str, Path] = {}
    for workload in workloads:
        spec = counterpart(workload, "linux_wasmtime")
        if spec is None:
            continue
        if "wasm_path" in spec:
            sources[spec["wasm_path"]] = root / spec["wasm_path"]
        for template in counterpart_templates(workload, "linux_wasmtime"):
            for match in GUEST_PATH_PATTERN.finditer(template):
                if match.group(1) == "cwasm":
                    sources[match.group(2)] = root / match.group(2)
    return [sources[key] for key in sorted(sources)]


def cwasm_path(source: Path) -> Path:
    return source.with_name(source.name + ".cwasm")


def needs_native_bin(workloads: list[dict]) -> bool:
    return any(
        uses_placeholder(workload, side, "{native_bin}") for workload in workloads for side in SIDES
    )


def render(template: str, context: RenderContext, workload: dict) -> str:
    rendered = template
    for placeholder, value in LINUX_TOOLS.items():
        rendered = rendered.replace(placeholder, value)
    rendered = rendered.replace("{native_bin}", str(context.repo_root / NATIVE_BIN_DIR))
    rendered = rendered.replace("{wasmtime}", context.wasmtime_bin)
    rendered = rendered.replace("{repo_root}", str(context.repo_root))
    rendered = rendered.replace("{workdir}", str(context.workdir))

    def guest_path(match: re.Match[str]) -> str:
        path = context.repo_root / match.group(2)
        return str(cwasm_path(path) if match.group(1) == "cwasm" else path)

    rendered = GUEST_PATH_PATTERN.sub(guest_path, rendered)
    hosts = context.hosts
    if "{host_http_url}" in rendered or "{host_http_large_url}" in rendered:
        if not hosts.http_url:
            raise SystemExit(f"workload {workload['name']} requires --host-http-url")
        rendered = rendered.replace("{host_http_url}", hosts.http_url)
        rendered = rendered.replace("{host_http_large_url}", large_http_url(hosts.http_url))
    if "{host_tcp_host}" in rendered:
        if not hosts.tcp_host:
            raise SystemExit(f"workload {workload['name']} requires --host-tcp-host")
        rendered = rendered.replace("{host_tcp_host}", hosts.tcp_host)
    if "{host_tcp_port}" in rendered:
        if hosts.tcp_port is None:
            raise SystemExit(f"workload {workload['name']} requires --host-tcp-port")
        rendered = rendered.replace("{host_tcp_port}", str(hosts.tcp_port))
    if "{host_tcp_echo_port}" in rendered:
        if hosts.tcp_echo_port is None:
            raise SystemExit(f"workload {workload['name']} requires --host-tcp-echo-port")
        rendered = rendered.replace("{host_tcp_echo_port}", str(hosts.tcp_echo_port))
    return rendered


def render_argv(program: str, args: list[str], context: RenderContext, workload: dict) -> list[str]:
    argv = shlex.split(render(program, context, workload))
    for arg in args:
        if arg == WASMTIME_RUN_PLACEHOLDER:
            argv.extend([context.wasmtime_bin, "run", "--allow-precompiled"])
        else:
            argv.append(render(arg, context, workload))
    return argv


def counterpart_command(workload: dict, side: str, context: RenderContext) -> list[str]:
    spec = counterpart(workload, side)
    if spec is None:
        raise SystemExit(f"workload {workload['name']} has no {side} counterpart")
    if "wasm_path" in spec:
        argv = [context.wasmtime_bin, "run"]
        argv.extend(render(flag, context, workload) for flag in spec.get("wasmtime_flags", []))
        argv.extend(["--allow-precompiled", str(cwasm_path(context.repo_root / spec["wasm_path"]))])
        argv.extend(render(arg, context, workload) for arg in spec.get("args", []))
        return argv
    shape = _shape(workload, spec)
    if "command" in shape:
        return ["/bin/sh", "-c", render(shape["command"], context, workload)]
    if "program" in shape:
        return render_argv(shape["program"], shape.get("args", []), context, workload)
    raise SystemExit(f"workload {workload['name']} {side} counterpart declares no command shape")


def parse_metrics(stdout: str, workload_name: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    for line in stdout.splitlines():
        if not line.startswith(METRIC_LINE_PREFIX):
            continue
        assignment = line[len(METRIC_LINE_PREFIX) :]
        name, separator, value = assignment.partition("=")
        if not separator:
            raise SystemExit(f"workload {workload_name} printed a metric line without `=`: {line!r}")
        try:
            number = float(value.strip())
        except ValueError as error:
            raise SystemExit(f"workload {workload_name} printed a non-numeric metric: {line!r}") from error
        name = name.strip()
        if name in metrics:
            raise SystemExit(f"workload {workload_name} reported metric {name!r} twice")
        metrics[name] = number
    return metrics


def validate_output(workload: dict, stdout: bytes, stderr: bytes) -> None:
    stdout_text = stdout.decode("utf-8", errors="replace")
    for expected in workload.get("stdout_contains", []):
        if expected not in stdout_text:
            sys.stdout.buffer.write(stdout)
            sys.stderr.buffer.write(stderr)
            raise SystemExit(
                f"workload {workload['name']} stdout did not contain expected text {expected!r}"
            )
    if workload.get("stderr_empty", False) and stderr:
        sys.stdout.buffer.write(stdout)
        sys.stderr.buffer.write(stderr)
        raise SystemExit(f"workload {workload['name']} wrote stderr")


class WorkloadFailed(Exception):
    """One workload could not be measured; the message says why."""


def run_once(workload: dict, argv: list[str], env: dict[str, str]) -> tuple[float, dict[str, float]]:
    """Runs one iteration and returns the child's wall time in milliseconds."""
    started = time.perf_counter_ns()
    completed = subprocess.run(argv, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    if completed.returncode != 0:
        sys.stdout.buffer.write(completed.stdout)
        sys.stderr.buffer.write(completed.stderr)
        raise WorkloadFailed(f"workload {workload['name']} exited with code {completed.returncode}")
    validate_output(workload, completed.stdout, completed.stderr)
    metrics = parse_metrics(completed.stdout.decode("utf-8", errors="replace"), workload["name"])
    return elapsed_ms, metrics


def median(values: list[float]) -> float:
    ordered = sorted(values)
    lower = (len(ordered) - 1) // 2
    upper = len(ordered) // 2
    return (ordered[lower] + ordered[upper]) / 2


def run_workloads(
    manifest_path: Path,
    names: list[str],
    side: str,
    iterations: int,
    context: RenderContext,
    output: Path,
    keep_going: bool,
) -> None:
    manifest = load_manifest(manifest_path)
    env = os.environ.copy()
    env["HELIOS_PROCESS_ID"] = str(os.getpid())
    with output.open("w", encoding="utf-8") as handle:
        handle.write(
            json.dumps(
                {
                    "type": "run",
                    "schema_version": SCHEMA_VERSION,
                    "side": side,
                    "manifest": str(manifest_path),
                    "iterations": iterations,
                    "selected_workloads": names,
                    "wasmtime_bin": context.wasmtime_bin,
                }
            )
            + "\n"
        )
        for name in names:
            workload = selected_workload(manifest, name)
            argv = counterpart_command(workload, side, context)
            elapsed: list[float] = []
            failure: str | None = None
            for iteration in range(1, iterations + 1):
                try:
                    elapsed_ms, metrics = run_once(workload, argv, env)
                except WorkloadFailed as error:
                    if not keep_going:
                        raise SystemExit(str(error)) from error
                    failure = str(error)
                    break
                elapsed.append(elapsed_ms)
                handle.write(
                    json.dumps(
                        {
                            "type": "iteration",
                            "workload": name,
                            "class": workload["class"],
                            "headline": workload.get("headline", False),
                            "side": side,
                            "iteration": iteration,
                            "elapsed_ms": elapsed_ms,
                            "metrics": metrics,
                            "command": argv,
                        }
                    )
                    + "\n"
                )
                handle.flush()
            if failure is not None:
                print(f"{failure}; recorded and continuing", file=sys.stderr)
                handle.write(
                    json.dumps(
                        {
                            "type": "failure",
                            "workload": name,
                            "class": workload["class"],
                            "headline": workload.get("headline", False),
                            "side": side,
                            "error": failure,
                        }
                    )
                    + "\n"
                )
                handle.flush()
                continue
            handle.write(
                json.dumps(
                    {
                        "type": "summary",
                        "workload": name,
                        "class": workload["class"],
                        "headline": workload.get("headline", False),
                        "side": side,
                        "median_elapsed_ms": median(elapsed),
                        "iterations": iterations,
                        "elapsed_ms": elapsed,
                    }
                )
                + "\n"
            )


def precompile(manifest_path: Path, names: list[str], wasmtime_bin: str, root: Path) -> None:
    manifest = load_manifest(manifest_path)
    workloads = [selected_workload(manifest, name) for name in names] if names else manifest["workloads"]
    for source in precompile_sources(root, workloads):
        target = cwasm_path(source)
        if target.exists() and target.stat().st_mtime >= source.stat().st_mtime:
            continue
        subprocess.run([wasmtime_bin, "compile", "-o", str(target), str(source)], check=True)
        print(f"precompiled {source} -> {target}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--manifest", type=Path, default=repo_root() / "tools/wasi-apps/workloads.json")
    parser.add_argument("--repo-root", type=Path, default=repo_root())
    parser.add_argument("--wasmtime-bin", default="wasmtime")
    subcommands = parser.add_subparsers(dest="command", required=True)

    run_parser = subcommands.add_parser("run")
    run_parser.add_argument("--workload", dest="workloads", action="append", required=True)
    run_parser.add_argument("--side", choices=SIDES, required=True)
    run_parser.add_argument("--iterations", type=int, default=1)
    run_parser.add_argument("--out", type=Path, required=True)
    run_parser.add_argument("--host-http-url")
    run_parser.add_argument("--host-tcp-host")
    run_parser.add_argument("--host-tcp-port", type=int)
    run_parser.add_argument("--host-tcp-echo-port", type=int)
    run_parser.add_argument(
        "--keep-going",
        action="store_true",
        help="record a workload that fails as a failure record and continue with the next one",
    )

    precompile_parser = subcommands.add_parser("precompile")
    precompile_parser.add_argument("--workload", dest="workloads", action="append", default=[])

    paths_parser = subcommands.add_parser("guest-paths")
    paths_parser.add_argument("--workload", dest="workloads", action="append", default=[])

    args = parser.parse_args()
    root = args.repo_root.resolve()
    if args.command == "run":
        if args.iterations <= 0:
            raise SystemExit("--iterations must be a positive integer")
        context = RenderContext(
            repo_root=root,
            wasmtime_bin=args.wasmtime_bin,
            hosts=HostEndpoints(args.host_http_url, args.host_tcp_host, args.host_tcp_port, args.host_tcp_echo_port),
            workdir=Path(tempfile.mkdtemp(prefix="helios-bench-")),
        )
        run_workloads(
            args.manifest, args.workloads, args.side, args.iterations, context, args.out, args.keep_going
        )
    elif args.command == "precompile":
        precompile(args.manifest, args.workloads, args.wasmtime_bin, root)
    else:
        manifest = load_manifest(args.manifest)
        workloads = (
            [selected_workload(manifest, name) for name in args.workloads]
            if args.workloads
            else manifest["workloads"]
        )
        for path in guest_paths(root, workloads):
            print(path.relative_to(root))


if __name__ == "__main__":
    main()
