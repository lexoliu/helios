"""The Helios side runs inside a budget, and a wedged guest costs one class.

Run 33952047436 stopped answering after its `net` class and held the
runner for ninety-five minutes with QEMU still alive behind it, until CI
cancelled the job. Every bound that would have caught it is exercised
here against a stand-in for `workload-bench.sh` that never returns.
"""

from __future__ import annotations

import os
import time
from pathlib import Path

import pytest
from bench_stub import WEDGED, WORKLOADS, fake_checkout, records

from helios_bench.wasi_apps import gap_bench


@pytest.fixture
def driver(tmp_path, monkeypatch):
    """The driver, pointed at a repository whose bench script we control."""
    module = gap_bench()
    fake_checkout(tmp_path)
    monkeypatch.setattr(module, "repo_root", lambda: tmp_path)
    monkeypatch.setenv("HELIOS_TEST_WEDGE_PID_FILE", str(tmp_path / "wedge.pid"))
    monkeypatch.setenv("HELIOS_TEST_BUILD_LOG", str(tmp_path / "builds"))
    monkeypatch.setenv("HELIOS_TEST_BUILD_SECONDS", "0")
    return module


def run_side(
    driver,
    out_dir: Path,
    per_class: int,
    side: int,
    workloads: list[dict] | None = None,
) -> tuple[Path, float]:
    out_dir.mkdir()
    started = time.monotonic()
    log = driver.run_helios(
        Path("tools/wasi-apps/workloads.json"),
        [driver.HeliosImage(name="helios", workspace_root=driver.repo_root(), out_dir=out_dir)],
        1,
        WORKLOADS if workloads is None else workloads,
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


def test_the_build_is_not_charged_to_any_class(driver, tmp_path, monkeypatch) -> None:
    """#153: a cold cargo cache is not a workload.

    The stand-in spends the whole side's budget building. Charged to the
    classes, that leaves the first one nothing — which is how
    `bench-x86-64-linux` died on a cold runner, with the budget spent
    before a guest had booted. Hoisted out of the budget, both classes
    below are measured. No class here wedges: what is under test is the
    build, and a deadline of its own would only make the assertion
    depend on how fast the runner is.
    """
    monkeypatch.setenv("HELIOS_TEST_BUILD_SECONDS", "4")
    measurable = [entry for entry in WORKLOADS if entry["class"] != WEDGED]

    log, elapsed = run_side(driver, tmp_path / "out", per_class=60, side=4, workloads=measurable)

    assert (tmp_path / "builds").read_text(encoding="utf-8").strip() == "built", (
        "the side must build once, before its budget starts"
    )
    assert elapsed > 4, "the build is the one thing that runs before the budget"
    assert all(record["type"] == "summary" for record in records(log).values()), (
        "no class may lose its share of the budget to the build"
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
