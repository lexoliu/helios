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


def test_failure_records_become_failed_cells(tmp_path, baseline_report: Report) -> None:
    from helios_bench.assemble import build_workload
    from helios_bench.render import render_tables
    from helios_bench.report import Side, WorkloadClass, WorkloadResult
    from helios_bench.sources import read_side_jsonl

    path = tmp_path / "helios.jsonl"
    path.write_text(
        '{"type": "run"}\n'
        '{"type": "failure", "workload": "tcp-throughput", "class": "net", "headline": true,'
        ' "error": "TcpErrorKind::Timeout: TCP read timed out"}\n',
        encoding="utf-8",
    )
    raw = read_side_jsonl(path, warmup_discard=1)
    assert raw.cells["tcp-throughput"].failure == "TcpErrorKind::Timeout: TCP read timed out"
    assert raw.cells["tcp-throughput"].iterations == []

    workload = build_workload(
        {"name": "tcp-throughput", "class": "net", "headline": True, "description": "TCP stream"},
        {},
        0.0,
        {Side.HELIOS: raw.cells["tcp-throughput"].failure},
    )
    assert isinstance(workload, WorkloadResult)
    assert workload.workload_class is WorkloadClass.NET
    assert workload.comparisons == []
    report = baseline_report.model_copy(update={"workloads": [*baseline_report.workloads, workload]})
    text = render_tables(report)
    assert "| `tcp-throughput` (headline) | **failed** | n/a | n/a | n/a | n/a |" in text
    assert "`tcp-throughput` failed on Helios: TcpErrorKind::Timeout: TCP read timed out" in text
