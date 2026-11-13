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
        hosts=runner.HostEndpoints(
            "http://10.0.2.2:80/payload.txt",
            "10.0.2.2",
            tcp_port=5000,
            tcp_upload_port=5002,
            tcp_echo_port=5001,
        ),
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
    assert "linux_wasmtime" in workload["uncompared"]
    with pytest.raises(SystemExit):
        runner.counterpart_command(workload, "linux_wasmtime", context)


def test_process_startup_times_wasmtime_spawn_the_same_way(context) -> None:
    runner = workload_runner()
    workload = runner.selected_workload(load_workloads(), "process-startup")
    argv = runner.counterpart_command(workload, "linux_wasmtime", context)
    assert argv[:2] == ["/bin/sh", "-c"]
    command = argv[2]
    assert "while [ $i -lt 20 ]" in command
    wasmtime = str(GUEST_ROOT / "tools/wasmtime")
    hello = str(GUEST_ROOT / "artifacts/wasi-tools/hello.wasm.cwasm")
    assert f"{wasmtime} run --allow-precompiled {hello}" in command


def test_the_shell_fs_and_ipc_rows_run_the_stubbed_coreutils(context) -> None:
    """`stdio-pipe`, `fs-smallfiles` and `fs-readstream` share one recipe:
    the same coreutils module with its WASIX imports stubbed, driven under
    `wasmtime run --dir` so the file work lands on the WASI filesystem."""
    runner = workload_runner()
    wasmtime = str(GUEST_ROOT / "tools/wasmtime")
    module = str(GUEST_ROOT / "artifacts/wasix/coreutils/coreutils-wasi.wasm.cwasm")
    for name in ("stdio-pipe", "fs-smallfiles", "fs-readstream"):
        workload = runner.selected_workload(load_workloads(), name)
        argv = runner.counterpart_command(workload, "linux_wasmtime", context)
        assert argv[:2] == ["/bin/sh", "-c"]
        command = argv[2]
        assert f"{wasmtime} run --allow-precompiled -W shared-memory" in command
        assert "--dir /tmp/work::/work" in command
        assert module in command
    stdio = runner.selected_workload(load_workloads(), "stdio-pipe")
    command = runner.counterpart_command(stdio, "linux_wasmtime", context)[2]
    assert f"--dir {GUEST_ROOT}/artifacts/wasix/bash::/input" in command
    assert "/input/bash.wasm" in command


def test_tcp_rows_share_the_wasi_client_with_the_row_label(context) -> None:
    runner = workload_runner()
    client = str(GUEST_ROOT / "artifacts/wasi-tools/wasi-tcp-throughput.wasm.cwasm")
    for name, tail in (
        ("tcp-throughput", ["10.0.2.2", "5000", "67108864", "--label", "tcp-throughput"]),
        ("tcp-upload", ["10.0.2.2", "5002", "67108864", "up", "--label", "tcp-upload"]),
        ("wasix-tcp-throughput", ["10.0.2.2", "5000", "67108864", "--label", "wasix-tcp-throughput"]),
    ):
        workload = runner.selected_workload(load_workloads(), name)
        argv = runner.counterpart_command(workload, "linux_wasmtime", context)
        assert client in argv, name
        assert "inherit-network=y" in argv, name
        assert argv[-len(tail) :] == tail, name


def test_the_curl_rows_run_the_plain_wasi_http_client(context) -> None:
    runner = workload_runner()
    client = str(GUEST_ROOT / "artifacts/wasi-tools/wasi-curl.wasm.cwasm")
    workload = runner.selected_workload(load_workloads(), "curl-local-http")
    argv = runner.counterpart_command(workload, "linux_wasmtime", context)
    assert client in argv
    assert "inherit-network=y" in argv
    assert argv[-1] == "http://10.0.2.2:80/payload.txt"

    throughput = runner.selected_workload(load_workloads(), "curl-http-throughput")
    argv = runner.counterpart_command(throughput, "linux_wasmtime", context)
    assert client in argv
    assert argv[-5:] == [
        "--output",
        "/dev/null",
        "--write-out",
        "curl-http-throughput:%{size_download}\\n",
        "http://10.0.2.2:80/payload-64m.bin",
    ]


def test_precompile_sources_cover_every_wasm_the_wasmtime_side_runs() -> None:
    runner = workload_runner()
    sources = {
        path.relative_to(REPO_ROOT).as_posix()
        for path in runner.precompile_sources(REPO_ROOT, load_workloads()["workloads"])
    }
    assert "artifacts/wasi-tools/hello.wasm" in sources
    assert "artifacts/wasi-tools/wasi-curl.wasm" in sources
    assert "artifacts/wasi-tools/wasi-tcp-throughput.wasm" in sources
    assert "artifacts/wasix/coreutils/coreutils-wasi.wasm" in sources
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
        5002,
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
    assert (args.host_tcp_port, args.host_tcp_upload_port, args.host_tcp_echo_port) == (5000, 5002, 5001)


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


def fake_wasmtime(tmp_path: Path, settings_output: str) -> Path:
    """A stand-in `wasmtime` whose `settings --json` prints `settings_output`."""
    script = tmp_path / "wasmtime"
    script.write_text(f"#!/bin/sh\ncat <<'EOF'\n{settings_output}\nEOF\n", encoding="utf-8")
    script.chmod(0o755)
    return script


def test_the_linux_wasmtime_run_record_names_the_isa_flags_wasmtime_inferred(tmp_path: Path) -> None:
    settings = json.dumps(
        {"triple": "x86_64-unknown-linux-gnu", "inferred": ["has_sse3", "has_avx", "has_avx2"]}
    )
    runner = workload_runner()
    assert runner.inferred_isa_flags(str(fake_wasmtime(tmp_path, settings))) == [
        "has_avx",
        "has_avx2",
        "has_sse3",
    ]


def test_a_wasmtime_that_infers_nothing_is_refused(tmp_path: Path) -> None:
    runner = workload_runner()
    with pytest.raises(SystemExit, match="no `inferred` flag list"):
        runner.inferred_isa_flags(str(fake_wasmtime(tmp_path, json.dumps({"triple": "x86_64"}))))


def test_the_run_record_of_the_wasmtime_side_carries_the_flags_and_the_native_side_does_not(
    tmp_path: Path,
) -> None:
    settings = json.dumps({"inferred": ["has_sse3"]})
    wasmtime = fake_wasmtime(tmp_path, settings)
    manifest = {
        "schema_version": 2,
        "control_workload": "prints-ok",
        "workloads": [
            {
                "name": "prints-ok",
                "class": "compute",
                "headline": False,
                "runner": "shell",
                "description": "prints ok",
                "command": "echo ok",
                "stdout_contains": ["ok"],
                "stderr_empty": True,
                "counterparts": {"linux_native": {"inherit": True}, "linux_wasmtime": {"inherit": True}},
            }
        ],
    }
    path = tmp_path / "workloads.json"
    path.write_text(json.dumps(manifest), encoding="utf-8")
    runner = workload_runner()
    context = runner.RenderContext(
        repo_root=tmp_path, wasmtime_bin=str(wasmtime), hosts=runner.HostEndpoints(), workdir=tmp_path
    )
    records = {}
    for side in ("linux_native", "linux_wasmtime"):
        out = tmp_path / f"{side}.jsonl"
        runner.run_workloads(path, ["prints-ok"], side, 1, context, out, True, None)
        records[side] = json.loads(out.read_text(encoding="utf-8").splitlines()[0])
    assert records["linux_wasmtime"]["inferred_isa_flags"] == ["has_sse3"]
    assert "inferred_isa_flags" not in records["linux_native"]


def test_the_aot_curl_host_renders_the_helios_columns_compile(context) -> None:
    runner = workload_runner()
    workload = runner.selected_workload(load_workloads(), "aot-curl")
    compiling = runner.RenderContext(
        repo_root=context.repo_root,
        wasmtime_bin=context.wasmtime_bin,
        hosts=context.hosts,
        workdir=context.workdir,
        compiler=runner.CompilerRequest("x86_64-unknown-none", 3, "x86-64-v4+vbmi+bitalg"),
    )
    assert runner.is_resident(workload, "linux_wasmtime")
    assert not runner.is_resident(workload, "linux_native")
    argv = runner.counterpart_command(workload, "linux_wasmtime", compiling)
    assert argv == [
        str(GUEST_ROOT / "native/helios-compiler-host"),
        "--plugin",
        str(GUEST_ROOT / "compiler_plugin.wasm"),
        "--input",
        str(GUEST_ROOT / "artifacts/wasi-tools/curl.wasm"),
        "--output",
        "/tmp/work/aot-curl-output.cwasm",
        "--target",
        "x86_64-unknown-none",
        "--isa-level",
        "x86-64-v4+vbmi+bitalg",
        "--hint",
        "performance",
        "--workers",
        "3",
    ]
    native = runner.counterpart_command(workload, "linux_native", compiling)
    assert native[:2] == [str(GUEST_ROOT / "tools/wasmtime"), "compile"]


def test_a_compile_request_the_run_was_not_given_is_refused(context) -> None:
    runner = workload_runner()
    workload = runner.selected_workload(load_workloads(), "aot-curl")
    with pytest.raises(SystemExit) as error:
        runner.counterpart_command(workload, "linux_wasmtime", context)
    assert "--compiler-target" in str(error.value)
    no_level = runner.RenderContext(
        repo_root=context.repo_root,
        wasmtime_bin=context.wasmtime_bin,
        hosts=context.hosts,
        workdir=context.workdir,
        compiler=runner.CompilerRequest("x86_64-unknown-none", 3, None),
    )
    with pytest.raises(SystemExit) as error:
        runner.counterpart_command(workload, "linux_wasmtime", no_level)
    assert "--compiler-isa-level" in str(error.value)
    no_workers = runner.RenderContext(
        repo_root=context.repo_root,
        wasmtime_bin=context.wasmtime_bin,
        hosts=context.hosts,
        workdir=context.workdir,
        compiler=runner.CompilerRequest("x86_64-unknown-none", None, "x86-64-v3"),
    )
    with pytest.raises(SystemExit) as error:
        runner.counterpart_command(workload, "linux_wasmtime", no_workers)
    assert "--compiler-workers" in str(error.value)


def test_the_stock_wasmtime_cli_is_provisioned_for_the_native_reference() -> None:
    """`aot-curl`'s native column runs `wasmtime compile`, so the guest needs
    the CLI for that side even though its Linux + Wasmtime column does not
    run it."""
    runner = workload_runner()
    workload = runner.selected_workload(load_workloads(), "aot-curl")
    assert runner.uses_wasmtime(workload, "linux_native")
    assert not runner.uses_wasmtime(workload, "linux_wasmtime")
    assert runner.wasmtime_users([workload]) == [workload]
    assert runner.needs_compiler_plugin([workload])
    quickjs = runner.selected_workload(load_workloads(), "quickjs-loop")
    assert runner.uses_wasmtime(quickjs, "linux_wasmtime")
    assert not runner.uses_wasmtime(quickjs, "linux_native")
    assert not runner.needs_compiler_plugin([quickjs])


def test_the_guest_runner_parses_the_compile_request_the_host_driver_passes() -> None:
    baseline = fedora_baseline()
    runner = workload_runner()
    command = baseline.runner_command(
        11,
        [{"name": "aot-curl"}],
        None,
        None,
        None,
        None,
        None,
        "linux-wasmtime.jsonl",
        "linux_wasmtime",
        "/home/bench/helios/tools/wasmtime",
        True,
        5400,
        runner.CompilerRequest("aarch64-unknown-none", 3, "aarch64-lse-fp16-dotprod"),
    )
    args = runner.build_parser().parse_args(shlex.split(command)[2:])
    assert (args.compiler_target, args.compiler_workers, args.compiler_isa_level) == (
        "aarch64-unknown-none",
        3,
        "aarch64-lse-fp16-dotprod",
    )


def resident_manifest(tmp_path: Path, die_after: int | None) -> Path:
    child = [
        sys.executable,
        str(Path(__file__).with_name("resident_child.py")),
        "--workers",
        "{compiler_workers}",
    ]
    if die_after is not None:
        child.extend(["--die-after", str(die_after)])
    manifest = {
        "schema_version": 2,
        "control_workload": "resident-host",
        "workloads": [
            {
                "name": "resident-host",
                "class": "compute",
                "headline": False,
                "runner": "helios-aot",
                "description": "a resident counterpart answering one request per iteration",
                "counterparts": {
                    "linux_native": None,
                    "linux_wasmtime": {"resident": True, "program": child[0], "args": child[1:]},
                },
            }
        ],
    }
    path = tmp_path / "workloads.json"
    path.write_text(json.dumps(manifest), encoding="utf-8")
    return path


def run_resident(tmp_path: Path, die_after: int | None) -> tuple[subprocess.CompletedProcess, list[dict]]:
    out = tmp_path / "linux-wasmtime.jsonl"
    completed = subprocess.run(
        [
            sys.executable,
            str(guest_runner_path()),
            "--manifest",
            str(resident_manifest(tmp_path, die_after)),
            "--repo-root",
            str(tmp_path),
            # The linux_wasmtime side's run record names the flags this
            # CLI infers, whichever workloads the side times.
            "--wasmtime-bin",
            str(fake_wasmtime(tmp_path, json.dumps({"inferred": ["has_sse3"]}))),
            "run",
            "--side",
            "linux_wasmtime",
            "--iterations",
            "4",
            "--out",
            str(out),
            "--keep-going",
            "--side-timeout-seconds",
            "60",
            "--compiler-workers",
            "3",
            "--workload",
            "resident-host",
        ],
        capture_output=True,
        text=True,
    )
    records = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines() if line.strip()]
    return completed, records


def test_a_resident_counterpart_is_timed_once_per_request(tmp_path) -> None:
    completed, records = run_resident(tmp_path, die_after=None)
    assert completed.returncode == 0, completed.stderr
    iterations = [record for record in records if record["type"] == "iteration"]
    assert [record["iteration"] for record in iterations] == [1, 2, 3, 4]
    assert all(record["elapsed_ms"] > 0 for record in iterations)
    assert all(record["metrics"] == {"compiler_workers": 3.0} for record in iterations)
    assert iterations[0]["command"][-2:] == ["--workers", "3"]
    (summary,) = [record for record in records if record["type"] == "summary"]
    assert summary["iterations"] == 4


def test_a_resident_counterpart_that_dies_becomes_a_failed_cell(tmp_path) -> None:
    completed, records = run_resident(tmp_path, die_after=2)
    assert completed.returncode == 0, completed.stderr
    assert [record["type"] for record in records if "workload" in record] == [
        "iteration",
        "iteration",
        "failure",
    ]
    failure = records[-1]
    assert "exited with code 3" in failure["error"]
    assert "stand-in host failed on purpose" in failure["error"]
