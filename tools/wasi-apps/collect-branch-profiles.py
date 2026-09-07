#!/usr/bin/env python3
"""Record branch profiles for the compute-parity artifacts, on Helios.

The loop this script drives is described in docs/pgo.md section (b):

1. `helios-branch-hints instrument` rewrites the artifact so every `if` and
   `br_if` counts which way it went;
2. the instrumented artifact is staged in place of the real one, so the
   kernel image the inspector builds carries it as a boot program;
3. `helios-inspector vm ... shell -c <workload>` boots the guest and runs
   the workload's own command line, and the guest program writes its
   counters to stdout at exit, which is the channel the inspector already
   brings back;
4. `helios-branch-hints record` sums the runs into the profile
   `tools/wasi-apps/build.sh` writes back into the rebuilt artifact.

The workload command lines are read from workloads.json rather than
repeated here, so a profile is recorded from the same work the benchmark
measures.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import shlex
import shutil
import signal
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
WORKLOADS = REPO_ROOT / "tools" / "wasi-apps" / "workloads.json"
PROFILE_DIR = REPO_ROOT / "tools" / "wasi-apps" / "branch-profiles"

# The guest paths inspector/src/workload_bench.rs resolves the workload
# program placeholders to.
GUEST_PROGRAMS = {
    "{python3}": "/bin/python3",
    "{quickjs}": "/bin/qjs",
    "{simd_lanes}": "/bin/simd-lanes",
}


@dataclass(frozen=True)
class Target:
    """One artifact to profile and the workloads that exercise it."""

    name: str
    artifact: Path
    placeholder: str


TARGETS = {
    "quickjs": Target(
        "quickjs", REPO_ROOT / "artifacts" / "wasix" / "quickjs" / "qjs.wasm", "{quickjs}"
    ),
    "python3": Target(
        "python3", REPO_ROOT / "artifacts" / "python3-root" / "python3.wasm", "{python3}"
    ),
}


def workloads_for(placeholder: str) -> list[dict]:
    definitions = json.loads(WORKLOADS.read_text())["workloads"]
    return [
        workload
        for workload in definitions
        if workload.get("class") == "compute" and workload.get("program") == placeholder
    ]


def guest_command(workload: dict) -> str:
    program = GUEST_PROGRAMS[workload["program"]]
    return " ".join([program, *(shlex.quote(arg) for arg in workload.get("args", []))])


def run(command: list[str], **kwargs) -> subprocess.CompletedProcess:
    print("+ " + " ".join(shlex.quote(part) for part in command), flush=True)
    kwargs.setdefault("cwd", REPO_ROOT)
    return subprocess.run(command, check=True, **kwargs)


@contextlib.contextmanager
def staged(artifact: Path, replacement: Path):
    """Puts `replacement` where the boot image picks the artifact up.

    Some artifact directories are symlinks into another checkout that
    shares the downloaded tooling, and writing through one of those would
    edit a tree this run does not own. Where the artifact's directory is a
    symlink, the link is moved aside and a real directory takes its place
    for the duration, with every entry but the artifact itself symlinked
    back to the original.
    """
    parent = artifact.parent
    if parent.is_symlink():
        original = parent.resolve()
        aside = parent.with_name(parent.name + ".branch-hints-aside")
        parent.rename(aside)
        parent.mkdir()
        for entry in original.iterdir():
            if entry.name != artifact.name:
                (parent / entry.name).symlink_to(entry)
        shutil.copyfile(replacement, artifact)
        try:
            yield
        finally:
            shutil.rmtree(parent)
            aside.rename(parent)
    else:
        backup = replacement.with_name(replacement.name + ".original")
        shutil.copyfile(artifact, backup)
        shutil.copyfile(replacement, artifact)
        try:
            yield
        finally:
            shutil.copyfile(backup, artifact)


def collect(target: Target, args: argparse.Namespace, tool: Path, out_dir: Path) -> Path:
    workloads = workloads_for(target.placeholder)
    if not workloads:
        raise SystemExit(f"no compute workload runs {target.placeholder}")

    instrumented = out_dir / f"{target.name}-instrumented.wasm"
    sites = out_dir / f"{target.name}-sites.json"
    run(
        [
            str(tool),
            "instrument",
            "--input",
            str(target.artifact),
            "--output",
            str(instrumented),
            "--sites",
            str(sites),
        ]
    )

    captures: list[Path] = []
    with staged(target.artifact, instrumented):
        for workload in workloads:
            capture = out_dir / f"{target.name}-{workload['name']}.counts"
            command = [
                str(args.inspector),
                "vm",
                "--arch",
                args.arch,
                "--accel",
                args.accel,
                "--memory",
                args.memory,
            ]
            if args.release:
                command.append("--release")
            command += ["shell", "-c", guest_command(workload)]
            with capture.open("w") as sink:
                run(command, stdout=sink)
            captures.append(capture)

    PROFILE_DIR.mkdir(parents=True, exist_ok=True)
    profile = PROFILE_DIR / f"{target.name}.json"
    record = [
        str(tool),
        "record",
        "--sites",
        str(sites),
        "--module",
        str(target.artifact.relative_to(REPO_ROOT)),
        "--output",
        str(profile),
    ]
    for capture, workload in zip(captures, workloads, strict=True):
        record += ["--counts", str(capture), "--run", workload["name"]]
    run(record)
    return profile


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "targets",
        nargs="*",
        default=sorted(TARGETS),
        help="artifacts to profile (default: every compute-parity artifact)",
    )
    parser.add_argument("--arch", default="x86-64")
    parser.add_argument("--accel", default="kvm")
    parser.add_argument("--memory", default="4G")
    parser.add_argument(
        "--release",
        action="store_true",
        default=True,
        help="boot the release kernel, which is what the suite measures",
    )
    parser.add_argument("--debug-kernel", dest="release", action="store_false")
    parser.add_argument(
        "--inspector",
        default=REPO_ROOT / "target" / "release" / "helios-inspector",
        type=Path,
    )
    parser.add_argument(
        "--out-dir",
        default=REPO_ROOT / "target" / "branch-profiles",
        type=Path,
        help="where the instrumented artifacts and the raw captures are kept",
    )
    args = parser.parse_args()
    # A kill has to unwind through the staging, or the instrumented
    # artifact is left where the next build would pick it up.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))

    # Both binaries are rebuilt every time, never reused from whatever a
    # warm `target/` happens to hold: an inspector built in another
    # checkout resolves *that* checkout as its workspace root and would
    # build a guest image from the wrong tree.
    tool = REPO_ROOT / "target" / "release" / "helios-branch-hints"
    run(["cargo", "build", "--release", "-p", "helios-branch-hints"])
    run(["cargo", "build", "--release", "-p", "helios-inspector"])

    args.out_dir.mkdir(parents=True, exist_ok=True)
    for name in args.targets:
        if name not in TARGETS:
            raise SystemExit(f"unknown target {name}; known: {', '.join(sorted(TARGETS))}")
        profile = collect(TARGETS[name], args, tool, args.out_dir)
        print(f"wrote {profile.relative_to(REPO_ROOT)}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
