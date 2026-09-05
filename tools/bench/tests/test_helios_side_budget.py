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
if [ -n "${HELIOS_WORKLOAD_BENCH_BUILD_ONLY:-}" ]; then
    sleep 2
    echo built >> "$HELIOS_TEST_BUILD_LOG"
    exit 0
fi
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
    monkeypatch.setenv("HELIOS_TEST_BUILD_LOG", str(tmp_path / "builds"))
    return module


def records(log: Path) -> dict[str, dict]:
    return {
        json.loads(line)["workload"]: json.loads(line)
        for line in log.read_text(encoding="utf-8").splitlines()
        if line.strip()
    }


def run_side(driver, out_dir: Path, per_class: int, side: int) -> tuple[Path, float]:
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
        timeout_seconds=per_class,
        side_timeout_seconds=side,
        build_timeout_seconds=60,
        skip_build=False,
        control_workload=None,
        keep_going=True,
    )
    return log, time.monotonic() - started


def test_a_class_that_never_answers_is_killed_and_the_side_continues(driver, tmp_path) -> None:
    # A per-class cap well inside the side's budget: what is under test
    # is that the class is killed on its own deadline and the side keeps
    # going, not how the budget is shared.
    log, elapsed = run_side(driver, tmp_path / "out", per_class=2, side=120)

    assert elapsed < 60, "a killed class must not be waited on"

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


def test_a_spent_budget_boots_no_further_guest(driver, tmp_path) -> None:
    # The side's budget is a cap, not a target: with none of it left, a
    # class is a stated failure rather than another boot.
    log, elapsed = run_side(driver, tmp_path / "out", per_class=120, side=1)

    assert elapsed < 30
    written = records(log)
    assert {record["type"] for record in written.values()} == {"failure"}
    assert all("budget was spent" in record["error"] for record in written.values())
    assert not (tmp_path / "wedge.pid").exists(), "no guest may be booted on a spent budget"


def test_the_build_is_not_charged_to_any_class(driver, tmp_path) -> None:
    """#153: a cold cargo cache is not a workload.

    The stand-in spends two seconds building, more than any one class's
    share of the budget below. Charging that to the first class is what
    killed `bench-x86-64-linux` on a cold runner — it died with the
    budget spent before a guest had booted — so what is asserted here is
    that every class that can be measured still was, and that the one
    that failed failed on its own deadline rather than on the budget.
    """
    log, elapsed = run_side(driver, tmp_path / "out", per_class=2, side=4)

    assert (tmp_path / "builds").read_text(encoding="utf-8").strip() == "built", (
        "the side must build once, before its budget starts"
    )
    assert elapsed > 2, "the build is the one thing that runs before the budget"

    written = records(log)
    assert written["quickjs-loop"]["type"] == "summary"
    assert written["hostcall-loop"]["type"] == "summary"
    assert "timed out" in written["wasi-tcp-throughput"]["error"], (
        "the wedged class must fail on its own deadline, not on a budget the build spent"
    )


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
