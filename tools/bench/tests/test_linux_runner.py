"""The guest-side runner's manifest interpretation, exercised on the host."""

import json
import os
import shlex
import subprocess
import sys
from pathlib import Path

import pytest

from helios_bench import REPO_ROOT
from helios_bench.wasi_apps import fedora_baseline, workload_runner
from helios_bench.workloads import load_workloads

GUEST_ROOT = Path("/home/bench/helios")


@pytest.fixture
def context():
    runner = workload_runner()
    return runner.RenderContext(
        repo_root=GUEST_ROOT,
        wasmtime_bin=str(GUEST_ROOT / "tools/wasmtime"),
        hosts=runner.HostEndpoints("http://10.0.2.2:80/payload.txt", "10.0.2.2", 5000, 5001),
        workdir=Path("/tmp/work"),
    )


def test_inherited_program_uses_the_linux_tool_table(context) -> None:
    runner = workload_runner()
    workload = runner.selected_workload(load_workloads(), "quickjs-loop")
    argv = runner.counterpart_command(workload, "linux_native", context)
    assert argv[0] == "qjs" and argv[1] == "-e"


def test_wasmtime_counterpart_runs_precompiled_module(context) -> None:
    runner = workload_runner()
    workload = runner.selected_workload(load_workloads(), "tcp-latency")
    argv = runner.counterpart_command(workload, "linux_wasmtime", context)
    assert argv[:2] == [str(GUEST_ROOT / "tools/wasmtime"), "run"]
    assert "--allow-precompiled" in argv
    assert str(GUEST_ROOT / "artifacts/wasi-tools/tcp-latency.wasm.cwasm") in argv
    assert argv[-3:] == ["10.0.2.2", "5001", "5000"]


def test_wasmtime_run_placeholder_expands_into_argv(context) -> None:
    runner = workload_runner()
    workload = runner.selected_workload(load_workloads(), "pipe-pingpong")
    argv = runner.counterpart_command(workload, "linux_wasmtime", context)
    assert argv[0] == str(GUEST_ROOT / "native/procbench")
    child = argv.index(str(GUEST_ROOT / "tools/wasmtime"))
    assert argv[child : child + 3] == [str(GUEST_ROOT / "tools/wasmtime"), "run", "--allow-precompiled"]
    assert argv[child + 3].endswith("pipe-echo.wasm.cwasm")


def test_missing_counterpart_is_explicit(context) -> None:
    runner = workload_runner()
    workload = runner.selected_workload(load_workloads(), "sched-tasks")
    assert runner.counterpart(workload, "linux_wasmtime") is None
    with pytest.raises(SystemExit):
        runner.counterpart_command(workload, "linux_wasmtime", context)


def test_precompile_sources_cover_every_wasm_the_wasmtime_side_runs() -> None:
    runner = workload_runner()
    sources = {
        path.relative_to(REPO_ROOT).as_posix()
        for path in runner.precompile_sources(REPO_ROOT, load_workloads()["workloads"])
    }
    assert "artifacts/wasi-tools/hello.wasm" in sources
    assert "artifacts/wasix/quickjs/qjs.wasm" in sources
    assert "artifacts/python3-root/python3.wasm" in sources


def test_metric_lines_are_parsed_strictly() -> None:
    runner = workload_runner()
    assert runner.parse_metrics("x:1\nbench.a=1.5\nbench.b=2\n", "w") == {"a": 1.5, "b": 2.0}
    with pytest.raises(SystemExit):
        runner.parse_metrics("bench.broken\n", "w")
    with pytest.raises(SystemExit):
        runner.parse_metrics("bench.a=1\nbench.a=2\n", "w")


def test_the_guest_runner_parses_the_command_the_host_driver_builds() -> None:
    """The two halves of the Linux side only meet inside a guest.

    `runner_command` writes the argv and `linux_workload_runner` reads it,
    from two files that are edited together and whose disagreement shows
    up an hour into a CI run as a traceback on the far side of an ssh.
    """
    baseline = fedora_baseline()
    runner = workload_runner()
    workloads = [{"name": "quickjs-loop"}, {"name": "hostcall-loop"}]
    command = baseline.runner_command(
        11,
        workloads,
        "http://10.77.0.1:80/payload.txt",
        "10.77.0.1",
        5000,
        5001,
        "linux-native.jsonl",
        "linux_native",
        None,
        True,
        5400,
    )
    argv = shlex.split(command)
    assert argv[0] == "python3"
    args = runner.build_parser().parse_args(argv[2:])
    assert args.command == "run"
    assert args.side == "linux_native"
    assert args.iterations == 11
    assert args.keep_going is True
    assert args.side_timeout_seconds == 5400
    assert args.workloads == ["quickjs-loop", "hostcall-loop"]


def test_a_hung_workload_becomes_a_failed_cell_instead_of_holding_the_side() -> None:
    """The timeout path, exercised rather than merely declared.

    A child that never exits used to hold the whole Linux side until the
    caller's ssh gave up, losing every cell the side had already measured.
    """
    runner = workload_runner()
    workload = {"name": "hangs-forever", "stdout_contains": [], "stderr_empty": True}
    argv = [sys.executable, "-c", "import time; time.sleep(30)"]
    with pytest.raises(runner.WorkloadFailed) as failure:
        runner.run_once(workload, argv, dict(os.environ), 0.2)
    assert "hangs-forever" in str(failure.value)
    assert "budget" in str(failure.value)


def test_a_workload_that_finishes_inside_its_share_is_timed_normally() -> None:
    runner = workload_runner()
    workload = {"name": "prints-a-metric", "stdout_contains": ["ok"], "stderr_empty": True}
    argv = [sys.executable, "-c", "print('ok'); print('bench.rate=2.5')"]
    elapsed_ms, metrics = runner.run_once(workload, argv, dict(os.environ), 30.0)
    assert elapsed_ms > 0
    assert metrics == {"rate": 2.5}


def guest_runner_path() -> Path:
    return REPO_ROOT / "tools/wasi-apps/linux_workload_runner.py"


def synthetic_manifest(tmp_path: Path) -> Path:
    """Three cells: one that passes, one that fails validation, one after it.

    The third is the point. A cell that fails must not take the cells
    behind it down with it, and the log the driver reads back has to
    account for all three.
    """
    manifest = {
        "schema_version": 2,
        "control_workload": "prints-ok",
        "workloads": [
            {
                "name": "prints-ok",
                "class": "compute",
                "headline": False,
                "runner": "shell",
                "description": "prints what it must",
                "command": "echo synthetic:ok",
                "stdout_contains": ["synthetic:ok"],
                "stderr_empty": True,
                "counterparts": {"linux_native": {"inherit": True}, "linux_wasmtime": None},
            },
            {
                "name": "writes-stderr",
                "class": "compute",
                "headline": False,
                "runner": "shell",
                "description": "writes stderr it is not allowed to write",
                "command": "echo synthetic:ok; echo noise >&2",
                "stdout_contains": ["synthetic:ok"],
                "stderr_empty": True,
                "counterparts": {"linux_native": {"inherit": True}, "linux_wasmtime": None},
            },
            {
                "name": "runs-after-the-failure",
                "class": "compute",
                "headline": False,
                "runner": "shell",
                "description": "the cell behind the failing one",
                "command": "echo synthetic:ok",
                "stdout_contains": ["synthetic:ok"],
                "stderr_empty": True,
                "counterparts": {"linux_native": {"inherit": True}, "linux_wasmtime": None},
            },
        ],
    }
    path = tmp_path / "workloads.json"
    path.write_text(json.dumps(manifest), encoding="utf-8")
    return path


def test_a_failed_cell_leaves_the_side_green_and_accounted_for(tmp_path) -> None:
    """The contract the driver reads back: exit 0, and a record per cell.

    `time_side` turns any non-zero exit into a `CalledProcessError` and
    throws the side away, so a recorded failure must not be one.
    """
    out = tmp_path / "linux-native.jsonl"
    completed = subprocess.run(
        [
            sys.executable,
            str(guest_runner_path()),
            "--manifest",
            str(synthetic_manifest(tmp_path)),
            "--repo-root",
            str(tmp_path),
            "run",
            "--side",
            "linux_native",
            "--iterations",
            "2",
            "--out",
            str(out),
            "--keep-going",
            "--side-timeout-seconds",
            "60",
            "--workload",
            "prints-ok",
            "--workload",
            "writes-stderr",
            "--workload",
            "runs-after-the-failure",
        ],
        capture_output=True,
        text=True,
    )
    assert completed.returncode == 0, completed.stderr

    records = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines() if line.strip()]
    by_workload: dict[str, set[str]] = {}
    for record in records:
        if "workload" in record:
            by_workload.setdefault(record["workload"], set()).add(record["type"])
    assert by_workload["prints-ok"] == {"iteration", "summary"}
    assert by_workload["writes-stderr"] == {"failure"}
    assert by_workload["runs-after-the-failure"] == {"iteration", "summary"}
    failure = next(record for record in records if record["type"] == "failure")
    assert "wrote stderr" in failure["error"]
    assert failure["side"] == "linux_native"


def test_a_workload_named_after_its_process_gets_a_new_name_each_iteration(tmp_path) -> None:
    """`fs-smallfiles` names its scratch directory after the process.

    On Helios every iteration is a fresh process; here one process runs
    them all, and the second `mkdir` of the same directory wrote
    "File exists" to stderr and failed a cell that was working.
    """
    manifest = {
        "schema_version": 2,
        "control_workload": "makes-a-directory",
        "workloads": [
            {
                "name": "makes-a-directory",
                "class": "fs",
                "headline": False,
                "runner": "shell",
                "description": "scratch named after the process",
                "command": "mkdir {workdir}/scratch-$HELIOS_PROCESS_ID; echo scratch:ok",
                "stdout_contains": ["scratch:ok"],
                "stderr_empty": True,
                "counterparts": {"linux_native": {"inherit": True}, "linux_wasmtime": None},
            }
        ],
    }
    path = tmp_path / "workloads.json"
    path.write_text(json.dumps(manifest), encoding="utf-8")
    out = tmp_path / "linux-native.jsonl"
    completed = subprocess.run(
        [
            sys.executable,
            str(guest_runner_path()),
            "--manifest",
            str(path),
            "--repo-root",
            str(tmp_path),
            "run",
            "--side",
            "linux_native",
            "--iterations",
            "3",
            "--out",
            str(out),
            "--keep-going",
            "--workload",
            "makes-a-directory",
        ],
        capture_output=True,
        text=True,
    )
    assert completed.returncode == 0, completed.stderr
    records = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines() if line.strip()]
    assert [record["type"] for record in records].count("iteration") == 3
    assert not [record for record in records if record["type"] == "failure"]
