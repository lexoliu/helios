"""The guest-side runner's manifest interpretation, exercised on the host."""

import os
import shlex
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
