"""`tcp-latency` end to end, against the real host echo server.

Run 33959252438 lost this workload on all three sides at once: the driver
bound the echo server to the host loopback while every guest reaches the
host at the lane's `net_host` — 10.77.0.1 on the tap lane — so no side
could connect and the round-trip claim had no number in any column
(#150). Nothing between the manifest entry and the measured percentile is
exercised anywhere else, so a break in it stayed invisible until a lane
had spent forty minutes to report an exit status.

These tests run the workload's own Linux counterpart against the real
`tcp_echo_server` over the loopback, through the same argument rendering
and the same output validation the guest runner uses, so the same break
fails in `tooling` instead.
"""

from __future__ import annotations

import shutil
import socket
import subprocess
from pathlib import Path

import pytest

from helios_bench import REPO_ROOT
from helios_bench.wasi_apps import gap_bench, workload_runner
from helios_bench.workloads import load_workloads

WORKLOAD = "tcp-latency"
NATIVE_SOURCE = REPO_ROOT / "tools/bench/native/tcp-latency.c"
# The manifest asks for 5000 round trips and the validation checks that
# count, so the test measures exactly what a lane measures; on the
# loopback that is a fraction of a second.
ROUNDS = 5000


@pytest.fixture
def echo_server():
    """The driver's own echo server, started the way the driver starts it."""
    server, port = gap_bench().start_host_tcp_echo()
    try:
        yield server, port
    finally:
        server.shutdown()
        server.server_close()


@pytest.fixture
def native_client(tmp_path: Path) -> Path:
    """The Linux counterpart, built for this host.

    The lane cross-builds it with zig; here the host compiler is enough,
    because what is under test is the workload's protocol against the
    echo server rather than the guest's toolchain.
    """
    compiler = shutil.which("cc") or shutil.which("gcc") or shutil.which("clang")
    if compiler is None:
        pytest.skip("no host C compiler to build the tcp-latency counterpart with")
    binary = tmp_path / "native" / "tcp-latency"
    binary.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [
            compiler,
            "-O2",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-I",
            str(NATIVE_SOURCE.parent),
            "-o",
            str(binary),
            str(NATIVE_SOURCE),
        ],
        check=True,
    )
    return binary


def test_the_echo_server_binds_every_host_interface(echo_server) -> None:
    """A loopback-only bind is unreachable from every side at once."""
    server, port = echo_server
    assert server.server_address[0] == gap_bench().HOST_SERVER_BIND_ADDRESS
    with socket.create_connection(("127.0.0.1", port), timeout=5) as client:
        client.sendall(b"0123456789abcdef")
        assert client.recv(16) == b"0123456789abcdef"


def test_the_workload_measures_a_round_trip(echo_server, native_client, tmp_path) -> None:
    """The manifest's own argv, run against the echo server, reports a latency."""
    runner = workload_runner()
    _, port = echo_server
    workload = runner.selected_workload(load_workloads(), WORKLOAD)
    context = runner.RenderContext(
        repo_root=native_client.parent.parent,
        wasmtime_bin="wasmtime",
        hosts=runner.HostEndpoints(tcp_host="127.0.0.1", tcp_echo_port=port),
        workdir=tmp_path,
    )
    argv = runner.counterpart_command(workload, "linux_native", context)
    assert argv == [str(native_client), "127.0.0.1", str(port), str(ROUNDS)]

    completed = subprocess.run(argv, capture_output=True, check=False, timeout=120)
    assert completed.returncode == 0, completed.stderr.decode("utf-8", errors="replace")
    # The guest runner accepts a cell only when this passes, so the test
    # fails for the same reasons a lane would.
    runner.validate_output(workload, completed.stdout, completed.stderr)

    metrics = runner.parse_metrics(completed.stdout.decode("utf-8"), WORKLOAD)
    assert set(metrics) == {"rtt_p50_us", "rtt_p99_us", "rtt_max_us", "rtt_mean_us"}
    assert metrics["rtt_p50_us"] > 0.0
    assert metrics["rtt_p50_us"] <= metrics["rtt_p99_us"] <= metrics["rtt_max_us"]


def test_a_failed_cell_quotes_the_workload_stderr() -> None:
    """The failure record has to carry why, not only that."""
    runner = workload_runner()
    assert runner.quoted_stderr(b"") == "; its stderr was empty"
    assert runner.quoted_stderr(b"connect: Connection refused\n") == ("; stderr: connect: Connection refused")
    long = b"x" * (runner.STDERR_QUOTE_CHARS + 50)
    quoted = runner.quoted_stderr(long)
    assert quoted.startswith(f"; stderr (last {runner.STDERR_QUOTE_CHARS} chars): …")
    assert quoted.endswith("x" * runner.STDERR_QUOTE_CHARS)
