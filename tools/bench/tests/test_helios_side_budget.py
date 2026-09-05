"""The Helios side runs inside a budget, and a wedged guest costs one class.

Run 33952047436 stopped answering after its `net` class and held the
runner for ninety-five minutes with QEMU still alive behind it, until CI
cancelled the job. Every bound that would have caught it is exercised
here against a stand-in for `workload-bench.sh` that never returns.
"""

from __future__ import annotations

import json
import os
import time
from pathlib import Path

import pytest

from helios_bench.wasi_apps import gap_bench

WEDGED = "net"

# Stands in for `workload-bench.sh`: writes a summary line per workload
# and exits, unless it is serving the class that wedges, in which case it
# never returns and leaves a child behind to prove the whole process
# group is torn down rather than just the script.
FAKE_BENCH = """#!/bin/sh
set -eu
log="$HELIOS_WORKLOAD_BENCH_LOG"
mkdir -p "$(dirname "$log")"
if [ "$HELIOS_WORKLOAD_BENCH_CLASSES" = "@WEDGED@" ]; then
    sleep 600 &
    echo $! > "$HELIOS_TEST_WEDGE_PID_FILE"
    wait
fi
for name in $(echo "$HELIOS_WORKLOAD_BENCH_WORKLOADS" | tr ',' ' '); do
    printf '{"type":"summary","workload":"%s","class":"%s","headline":false,\
"runner":"program","median_elapsed_ms":1,"iterations":1,"elapsed_ms":[1],\
"validation":{"ok":true}}\\n' "$name" "$HELIOS_WORKLOAD_BENCH_CLASSES" >> "$log"
done
""".replace("@WEDGED@", WEDGED)


def workload(name: str, workload_class: str) -> dict:
    return {"name": name, "class": workload_class, "runner": "program", "headline": False}


WORKLOADS = [
    workload("quickjs-loop", "compute"),
    workload("wasi-tcp-throughput", WEDGED),
    workload("hostcall-loop", "hostcall"),
]


@pytest.fixture
def driver(tmp_path, monkeypatch):
    """The driver, pointed at a repository whose bench script we control."""
    module = gap_bench()
    script = tmp_path / "tools/wasi-apps/workload-bench.sh"
    script.parent.mkdir(parents=True)
    script.write_text(FAKE_BENCH, encoding="utf-8")
    script.chmod(0o755)
    monkeypatch.setattr(module, "repo_root", lambda: tmp_path)
    monkeypatch.setenv("HELIOS_TEST_WEDGE_PID_FILE", str(tmp_path / "wedge.pid"))
    return module


def records(log: Path) -> dict[str, dict]:
    return {
        json.loads(line)["workload"]: json.loads(line)
        for line in log.read_text(encoding="utf-8").splitlines()
        if line.strip()
    }


def test_a_class_that_never_answers_is_killed_and_the_side_continues(driver, tmp_path) -> None:
    out_dir = tmp_path / "out"
    out_dir.mkdir()
    started = time.monotonic()

    log = driver.run_helios(
        Path("tools/wasi-apps/workloads.json"),
        out_dir,
        1,
        WORKLOADS,
        "x86-64",
        "kvm",
        None,
        None,
        None,
        None,
        timeout_seconds=60,
        side_timeout_seconds=6,
        control_workload=None,
        keep_going=True,
    )
    elapsed = time.monotonic() - started

    assert elapsed < 6, "the side must not outlive its own budget"

    written = records(log)
    assert written["quickjs-loop"]["type"] == "summary"
    assert written["hostcall-loop"]["type"] == "summary", (
        "a class after the wedged one must still be measured"
    )
    wedged = written["wasi-tcp-throughput"]
    assert wedged["type"] == "failure"
    assert "timed out" in wedged["error"]

    wedge_pid = int((tmp_path / "wedge.pid").read_text(encoding="utf-8").strip())
    with pytest.raises(ProcessLookupError):
        os.kill(wedge_pid, 0)


def test_the_budget_is_shared_out_as_the_classes_run(driver) -> None:
    # Seven classes and an hour: each may take a seventh, and a class
    # that hands time back widens what is left for the rest.
    assert driver.class_budget(3600, 7, 9000) == 514
    assert driver.class_budget(3600, 1, 9000) == 3600
    # The per-class cap still wins when the budget is the larger number.
    assert driver.class_budget(3600, 1, 600) == 600
    # A spent budget buys no boot at all.
    assert driver.class_budget(-1, 3, 600) == 0
    assert driver.class_budget(3600, 0, 600) == 0
