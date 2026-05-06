#!/usr/bin/env python3
import argparse
import http.server
import json
import os
import platform
import shutil
import socketserver
import subprocess
import sys
import threading
import time
from pathlib import Path


HTTP_PAYLOAD_FILE = "payload.txt"
HTTP_LARGE_PAYLOAD_FILE = "payload-64m.bin"
HTTP_PAYLOAD = b"helios-linux-gap:ok\n"
HTTP_LARGE_PAYLOAD_BYTES = 64 * 1024 * 1024
HTTP_LARGE_PAYLOAD_CHUNK = bytes(range(251))
DEFAULT_GUEST_INTERVAL = "1ms"
DEFAULT_PERF_EVENT = "cycles:u"
PROFILE_DESCRIPTIONS = {
    "guest": {
        "profile_kind": "wasmtime-guest-profiler",
        "profile_scope": "wasm-guest",
        "description": "Wasmtime in-process guest sampling profile for wasm stacks, viewable in Firefox Profiler.",
    },
    "perfmap": {
        "profile_kind": "linux-perf-jit-symbols",
        "profile_scope": "native-and-jit",
        "description": "Linux perf profile with Wasmtime perfmap JIT code symbols for flamegraph generation.",
    },
    "jitdump": {
        "profile_kind": "linux-perf-jit-symbols",
        "profile_scope": "native-and-jit",
        "description": "Linux perf profile with Wasmtime jitdump records injected before flamegraph generation.",
    },
}


class QuietHttpHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, format: str, *args: object) -> None:
        return


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def load_manifest(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as handle:
        manifest = json.load(handle)
    if manifest.get("schema_version") != 1:
        raise SystemExit(f"unsupported workload manifest schema_version {manifest.get('schema_version')}")
    return manifest


def selected_workload(manifest: dict, name: str) -> dict:
    for workload in manifest["workloads"]:
        if workload["name"] == name:
            return workload
    raise SystemExit(f"unknown workload {name}")


def write_http_payloads(root: Path) -> None:
    root.mkdir(parents=True, exist_ok=True)
    (root / HTTP_PAYLOAD_FILE).write_bytes(HTTP_PAYLOAD)
    large_path = root / HTTP_LARGE_PAYLOAD_FILE
    remaining = HTTP_LARGE_PAYLOAD_BYTES
    with large_path.open("wb") as handle:
        while remaining:
            chunk = HTTP_LARGE_PAYLOAD_CHUNK[: min(remaining, len(HTTP_LARGE_PAYLOAD_CHUNK))]
            handle.write(chunk)
            remaining -= len(chunk)


def start_host_http(root: Path) -> tuple[socketserver.TCPServer, int]:
    handler = lambda *args, **kwargs: QuietHttpHandler(*args, directory=str(root), **kwargs)
    server = socketserver.TCPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, int(server.server_address[1])


def large_http_url(host_http_url: str) -> str:
    prefix, separator, _ = host_http_url.rpartition("/")
    if not separator:
        raise SystemExit(f"host HTTP URL has no path segment: {host_http_url}")
    return f"{prefix}/{HTTP_LARGE_PAYLOAD_FILE}"


def render_arg(value: str, host_http_url: str | None, workload_name: str) -> str:
    rendered = value
    rendered = rendered.replace("{repo_root}", str(repo_root()))
    if "{host_http_url}" in rendered:
        if not host_http_url:
            raise SystemExit(f"workload {workload_name} requires --host-http-url or --serve-local-http")
        rendered = rendered.replace("{host_http_url}", host_http_url)
    if "{host_http_large_url}" in rendered:
        if not host_http_url:
            raise SystemExit(f"workload {workload_name} requires --host-http-url or --serve-local-http")
        rendered = rendered.replace("{host_http_large_url}", large_http_url(host_http_url))
    return rendered


def profile_spec(workload: dict, host_http_url: str | None) -> tuple[Path, list[str], list[str]]:
    spec = workload.get("wasmtime_profile")
    if not spec:
        raise SystemExit(f"workload {workload['name']} has no wasmtime_profile entry")
    wasm_path = repo_root() / spec["wasm_path"]
    if not wasm_path.exists():
        raise SystemExit(f"wasmtime profile wasm artifact does not exist: {wasm_path}")
    flags = [
        render_arg(flag, host_http_url, workload["name"])
        for flag in spec.get("wasmtime_flags", [])
    ]
    args = [render_arg(arg, host_http_url, workload["name"]) for arg in spec.get("args", [])]
    return wasm_path, flags, args


def require_tool(name: str) -> str:
    path = shutil.which(name)
    if path is None:
        raise SystemExit(f"required tool is missing from PATH: {name}")
    return path


def run(command: list[str], cwd: Path, stdout_path: Path | None = None) -> None:
    if stdout_path:
        with stdout_path.open("wb") as handle:
            subprocess.run(command, cwd=cwd, stdout=handle, check=True)
    else:
        subprocess.run(command, cwd=cwd, check=True)


def write_metadata(path: Path, metadata: dict) -> None:
    path.write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def run_guest_profile(
    wasmtime_bin: str,
    wasm_path: Path,
    wasmtime_flags: list[str],
    wasm_args: list[str],
    out_dir: Path,
    interval: str,
) -> dict:
    profile_path = out_dir / "wasmtime-guest-profile.json"
    command = [
        wasmtime_bin,
        "run",
        *wasmtime_flags,
        f"--profile=guest,{profile_path},{interval}",
        str(wasm_path),
        *wasm_args,
    ]
    run(command, out_dir)
    return {
        "mode": "guest",
        **PROFILE_DESCRIPTIONS["guest"],
        "command": command,
        "firefox_profile_json": str(profile_path),
    }


def run_perf_profile(
    wasmtime_bin: str,
    wasm_path: Path,
    wasmtime_flags: list[str],
    wasm_args: list[str],
    out_dir: Path,
    mode: str,
    event: str,
    no_flamegraph: bool,
) -> dict:
    if platform.system() != "Linux":
        raise SystemExit(f"Wasmtime {mode} profiling through perf requires a Linux host")
    perf = require_tool("perf")
    perf_data = out_dir / "perf.data"
    profile_arg = f"--profile={mode}"
    record = [
        perf,
        "record",
        "-k",
        "mono",
        "-e",
        event,
        "-g",
        "-o",
        str(perf_data),
        "--",
        wasmtime_bin,
        "run",
        *wasmtime_flags,
        profile_arg,
        str(wasm_path),
        *wasm_args,
    ]
    run(record, out_dir)
    script_input = perf_data
    injected = None
    if mode == "jitdump":
        injected = out_dir / "perf.jit.data"
        run([perf, "inject", "-j", "-i", str(perf_data), "-o", str(injected)], out_dir)
        script_input = injected
    perf_script = out_dir / "perf.script"
    run([perf, "script", "-i", str(script_input)], out_dir, perf_script)
    metadata = {
        "mode": mode,
        **PROFILE_DESCRIPTIONS[mode],
        "command": record,
        "perf_data": str(perf_data),
        "perf_script": str(perf_script),
    }
    if injected:
        metadata["perf_jit_data"] = str(injected)
    if not no_flamegraph:
        collapse = require_tool("inferno-collapse-perf")
        flamegraph = require_tool("inferno-flamegraph")
        folded = out_dir / "perf.folded"
        svg = out_dir / "perf.svg"
        run([collapse, str(perf_script)], out_dir, folded)
        run([flamegraph, str(folded)], out_dir, svg)
        metadata["folded"] = str(folded)
        metadata["svg"] = str(svg)
    return metadata


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=repo_root() / "tools/wasi-apps/workloads.json")
    parser.add_argument("--workload", required=True)
    parser.add_argument(
        "--mode",
        choices=["guest", "perfmap", "jitdump"],
        default="guest",
        help="Wasmtime guest profiler or Linux perf-backed native/JIT symbol profiling mode.",
    )
    parser.add_argument("--out-dir", type=Path)
    parser.add_argument("--wasmtime-bin", default=os.environ.get("WASMTIME_BIN", "wasmtime"))
    parser.add_argument("--host-http-url")
    parser.add_argument("--serve-local-http", action="store_true")
    parser.add_argument("--guest-interval", default=DEFAULT_GUEST_INTERVAL)
    parser.add_argument("--perf-event", default=DEFAULT_PERF_EVENT)
    parser.add_argument("--no-flamegraph", action="store_true")
    args = parser.parse_args()

    manifest = load_manifest(args.manifest)
    workload = selected_workload(manifest, args.workload)
    out_dir = args.out_dir or repo_root() / "target/perf-baselines" / f"wasmtime-profile-{workload['name']}-{args.mode}-{int(time.time())}"
    if not out_dir.is_absolute():
        out_dir = repo_root() / out_dir
    out_dir = out_dir.resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    server = None
    host_http_url = args.host_http_url
    if args.serve_local_http:
        http_root = out_dir / "http-root"
        write_http_payloads(http_root)
        server, port = start_host_http(http_root)
        host_http_url = f"http://127.0.0.1:{port}/{HTTP_PAYLOAD_FILE}"

    try:
        wasm_path, wasmtime_flags, wasm_args = profile_spec(workload, host_http_url)
        if args.mode == "guest":
            metadata = run_guest_profile(
                args.wasmtime_bin,
                wasm_path,
                wasmtime_flags,
                wasm_args,
                out_dir,
                args.guest_interval,
            )
        else:
            metadata = run_perf_profile(
                args.wasmtime_bin,
                wasm_path,
                wasmtime_flags,
                wasm_args,
                out_dir,
                args.mode,
                args.perf_event,
                args.no_flamegraph,
            )
        metadata.update(
            {
                "workload": workload["name"],
                "wasm_path": str(wasm_path),
                "wasmtime_flags": wasmtime_flags,
                "wasm_args": wasm_args,
                "out_dir": str(out_dir),
                "host_http_url": host_http_url,
            }
        )
        metadata_path = out_dir / "wasmtime-profile.json"
        write_metadata(metadata_path, metadata)
        print(metadata_path)
    finally:
        if server:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
