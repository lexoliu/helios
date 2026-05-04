#!/usr/bin/env python3
import argparse
import json
import os
import shlex
import subprocess
import sys
from pathlib import Path


LINUX_PLACEHOLDERS = {
    "{bash}": "bash",
    "{cat}": "cat",
    "{curl}": "curl --fail --silent --show-error",
    "{head}": "head",
    "{mkdir}": "mkdir",
    "{python3}": "python3",
    "{quickjs}": "qjs",
    "{simd_lanes}": "python3 -c 'print(\"simd-lanes:17\")'",
}


def render_template(template: str, host_http_url: str | None, workload: dict) -> str:
    rendered = template
    for placeholder, value in LINUX_PLACEHOLDERS.items():
        rendered = rendered.replace(placeholder, value)
    if "{host_http_url}" in rendered:
        if not host_http_url:
            raise SystemExit(f"workload {workload['name']} requires --host-http-url")
        rendered = rendered.replace("{host_http_url}", host_http_url)
    return rendered


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


def render_command(workload: dict, host_http_url: str | None) -> str:
    if workload["runner"] != "shell":
        raise SystemExit(f"workload {workload['name']} is not a native Linux shell workload")
    command = workload.get("command")
    if not command:
        raise SystemExit(f"workload {workload['name']} is missing command")
    return render_template(command, host_http_url, workload)


def render_program(workload: dict, host_http_url: str | None) -> list[str]:
    if workload["runner"] != "program":
        raise SystemExit(f"workload {workload['name']} is not a native Linux program workload")
    program = workload.get("program")
    if not program:
        raise SystemExit(f"workload {workload['name']} is missing program")
    rendered = shlex.split(render_template(program, host_http_url, workload))
    rendered.extend(
        render_template(arg, host_http_url, workload) for arg in workload.get("args", [])
    )
    return rendered


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


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--workload", required=True)
    parser.add_argument("--host-http-url")
    args = parser.parse_args()

    manifest = load_manifest(args.manifest)
    workload = selected_workload(manifest, args.workload)
    env = os.environ.copy()
    env["HELIOS_PROCESS_ID"] = str(os.getpid())
    if workload["runner"] == "shell":
        completed = subprocess.run(
            ["/bin/sh", "-c", render_command(workload, args.host_http_url)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
    elif workload["runner"] == "program":
        completed = subprocess.run(
            render_program(workload, args.host_http_url),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
    else:
        raise SystemExit(f"workload {workload['name']} is not a native Linux workload")
    if completed.returncode != 0:
        sys.stdout.buffer.write(completed.stdout)
        sys.stderr.buffer.write(completed.stderr)
        raise SystemExit(
            f"workload {workload['name']} exited with code {completed.returncode}"
        )
    validate_output(workload, completed.stdout, completed.stderr)


if __name__ == "__main__":
    main()
