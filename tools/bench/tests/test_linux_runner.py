"""The guest-side runner's manifest interpretation, exercised on the host."""

from pathlib import Path

import pytest

from helios_bench import REPO_ROOT
from helios_bench.wasi_apps import workload_runner
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
