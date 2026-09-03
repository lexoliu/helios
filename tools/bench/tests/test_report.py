from pathlib import Path

import pytest

from helios_bench.report import Report, Side, WorkloadClass, load_report, save_report


def test_report_round_trips_through_json(tmp_path: Path, baseline_report: Report) -> None:
    path = tmp_path / "report.json"
    save_report(baseline_report, path)
    loaded = load_report(path)
    assert loaded == baseline_report


def test_cold_iteration_is_separated_from_the_warm_series(baseline_report: Report) -> None:
    cell = baseline_report.workload("hostcall-loop").cells[Side.HELIOS]
    assert cell.iterations[0].cold
    assert cell.cold.count == 1
    assert cell.warm.count == 10
    assert cell.cold.median > cell.warm.median
    assert cell.metrics["x"].median == 1.0


def test_comparisons_and_parity_verdicts(baseline_report: Report, advisory_report: Report) -> None:
    hostcall = baseline_report.workload("hostcall-loop")
    against = {comparison.against: comparison for comparison in hostcall.comparisons}
    assert against[Side.LINUX_WASMTIME].speedup == pytest.approx(10.0, rel=0.1)
    assert against[Side.LINUX_WASMTIME].significant and against[Side.LINUX_WASMTIME].beyond_noise
    assert against[Side.LINUX_NATIVE].speedup == pytest.approx(2.5, rel=0.1)
    assert not hostcall.parity_bug

    parity = baseline_report.workload("quickjs-loop")
    assert not parity.parity_bug
    slower = advisory_report.workload("quickjs-loop")
    assert slower.parity_bug

    fs = baseline_report.workload("fs-smallfiles")
    assert Side.LINUX_WASMTIME not in fs.cells
    assert [comparison.against for comparison in fs.comparisons] == [Side.LINUX_NATIVE]


def test_grouping_helpers(baseline_report: Report) -> None:
    grouped = baseline_report.by_class()
    assert list(grouped) == [WorkloadClass.HOSTCALL, WorkloadClass.FS, WorkloadClass.COMPUTE]
    assert [workload.name for workload in baseline_report.headline_workloads()] == [
        "hostcall-loop",
        "quickjs-loop",
    ]
    assert baseline_report.control is not None
    assert 0.0 < baseline_report.control.noise_floor < 0.1
