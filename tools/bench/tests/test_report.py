import json
from pathlib import Path

import pytest

from helios_bench.render import render_pins
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
    assert cell.metrics["rtt_p50_us"].median == pytest.approx(cell.warm.median * 10.0)


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
    assert not fs.parity_bug


def test_parity_bug_applies_to_every_compared_class() -> None:
    """A loss to Linux + Wasmtime is a bug on any class, not only compute.

    The rule is the same for every row that has a Linux + Wasmtime cell:
    a Helios median slower beyond the run's noise floor carries the flag.
    """
    from conftest import make_report

    report = make_report({"hostcall-loop": 300.0, "quickjs-loop": 100.5, "fs-smallfiles": 25.0})

    hostcall = report.workload("hostcall-loop")
    assert hostcall.workload_class is WorkloadClass.HOSTCALL
    assert hostcall.parity_bug

    quickjs = report.workload("quickjs-loop")
    against = {comparison.against: comparison for comparison in quickjs.comparisons}
    assert not against[Side.LINUX_WASMTIME].beyond_noise
    assert not quickjs.parity_bug

    fs = report.workload("fs-smallfiles")
    assert Side.LINUX_WASMTIME not in fs.cells
    assert not fs.parity_bug


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
    raw = read_side_jsonl(path, Side.HELIOS, "vsock", warmup_discard=1)
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


def test_a_report_written_before_the_isa_pins_still_loads(tmp_path: Path, baseline_report: Report) -> None:
    """Older reports carry neither the guest's ISA level nor the Linux flags."""
    path = tmp_path / "report.json"
    save_report(baseline_report, path)
    raw = json.loads(path.read_text())
    for field in ("guest_isa", "baseline_guest_isa", "linux_wasmtime_inferred_isa_flags"):
        raw["pins"].pop(field, None)
    path.write_text(json.dumps(raw))
    loaded = load_report(path)
    assert loaded.pins.guest_isa is None
    assert loaded.pins.linux_wasmtime_inferred_isa_flags is None
    rendered = render_pins(loaded)
    assert "Guest code ISA level" not in rendered
    assert "inferred ISA flags" not in rendered


def test_the_isa_pins_survive_a_round_trip(tmp_path: Path, baseline_report: Report) -> None:
    path = tmp_path / "report.json"
    save_report(baseline_report, path)
    loaded = load_report(path)
    assert loaded.pins.guest_isa is not None
    assert loaded.pins.guest_isa.level == "x86-64-v4"
    assert loaded.pins.guest_isa.extras == ["vbmi", "bitalg"]
    assert loaded.pins.linux_wasmtime_inferred_isa_flags == ["has_avx", "has_avx2", "has_avx512f", "has_sse3"]


def test_a_report_written_before_the_rpc_transport_was_recorded_still_loads(
    tmp_path: Path, baseline_report: Report
) -> None:
    """The gate compares a run against the lane's previous run, and the
    previous run's report may predate the field (#413): it loads, its
    transport reads as unrecorded, and nothing about the comparison
    depends on it."""
    path = tmp_path / "report.json"
    save_report(baseline_report, path)
    raw = json.loads(path.read_text())
    del raw["pins"]["rpc_transport"]
    path.write_text(json.dumps(raw))
    loaded = load_report(path)
    assert loaded.pins.rpc_transport is None
    assert "inspector RPC over `unrecorded`" in render_pins(loaded)


def test_a_report_written_before_matched_metrics_was_recorded_still_loads(
    tmp_path: Path, baseline_report: Report
) -> None:
    """A baseline run's report may predate the field (#849): it reads as having none."""
    path = tmp_path / "report.json"
    save_report(baseline_report, path)
    raw = json.loads(path.read_text())
    for workload in raw["workloads"]:
        workload.pop("matched_metrics", None)
    path.write_text(json.dumps(raw))
    loaded = load_report(path)
    assert all(workload.matched_metrics == () for workload in loaded.workloads)


AOT_CURL = {
    "name": "aot-curl",
    "class": "compute",
    "headline": False,
    "description": "the compiler plugin compiling curl.wasm",
    "matched_metrics": ["compiler_workers"],
    "uncompared": {"linux_native": "native `wasmtime compile` is the reference"},
}


def aot_iterations(center: float, seed: int, workers: float | None) -> list:
    from conftest import iterations

    measured = iterations(center, center * 0.02, seed)
    for iteration in measured:
        iteration.metrics = {} if workers is None else {"compiler_workers": workers}
    return measured


def aot_cells(helios_workers: float | None, linux_workers: float | None) -> dict:
    from conftest import THRESHOLDS

    from helios_bench.assemble import build_cell

    return {
        Side.HELIOS: build_cell(Side.HELIOS, aot_iterations(250.0, 3, helios_workers), THRESHOLDS),
        Side.LINUX_WASMTIME: build_cell(
            Side.LINUX_WASMTIME, aot_iterations(240.0, 4, linux_workers), THRESHOLDS
        ),
        Side.LINUX_NATIVE: build_cell(Side.LINUX_NATIVE, aot_iterations(100.0, 5, None), THRESHOLDS),
    }


def test_a_measured_uncompared_cell_is_a_reference_with_its_note(baseline_report: Report) -> None:
    from helios_bench.assemble import build_workload, check_matched_metrics
    from helios_bench.render import render_tables

    cells = aot_cells(3.0, 3.0)
    check_matched_metrics(AOT_CURL, cells)
    workload = build_workload(AOT_CURL, cells, 0.01)
    assert [comparison.against for comparison in workload.comparisons] == [Side.LINUX_WASMTIME]
    assert Side.LINUX_NATIVE in workload.cells

    report = baseline_report.model_copy(update={"workloads": [workload]})
    text = render_tables(report)
    row = next(line for line in text.splitlines() if line.startswith("| `aot-curl`"))
    columns = [column.strip() for column in row.strip("|").split("|")]
    assert len(columns) == 6, columns
    _, helios, wasmtime, native, vs_wasmtime, vs_native = columns
    # The native median is printed beside the others, and nothing is
    # divided by it.
    assert "[" in helios and "[" in wasmtime and "[" in native
    assert vs_wasmtime != "n/a"
    assert vs_native == "n/a"
    assert (
        "`aot-curl` is not compared on Native Linux (shown as a reference): "
        "native `wasmtime compile` is the reference"
    ) in text


def test_matched_metrics_that_differ_stop_assembly() -> None:
    from helios_bench.assemble import check_matched_metrics

    with pytest.raises(SystemExit) as error:
        check_matched_metrics(AOT_CURL, aot_cells(3.0, 4.0))
    message = str(error.value)
    assert "aot-curl" in message and "compiler_workers" in message
    assert "helios: [3.0]" in message and "linux_wasmtime: [4.0]" in message


def test_a_matched_metric_one_side_did_not_report_stops_assembly() -> None:
    from helios_bench.assemble import check_matched_metrics

    with pytest.raises(SystemExit) as error:
        check_matched_metrics(AOT_CURL, aot_cells(3.0, None))
    assert "linux_wasmtime iterations" in str(error.value)


def test_compiler_pins_round_trip_and_render(tmp_path: Path, baseline_report: Report) -> None:
    pins = baseline_report.pins.model_copy(
        update={"compiler_plugin_wasm": "a1" * 32, "compiler_host": "b2" * 32}
    )
    report = baseline_report.model_copy(update={"pins": pins})
    path = tmp_path / "report.json"
    save_report(report, path)
    loaded = load_report(path)
    assert loaded.pins.compiler_plugin_wasm == "a1" * 32
    assert loaded.pins.compiler_host == "b2" * 32
    text = render_pins(loaded)
    assert f"| Compiler plugin `compiler_plugin.wasm` | `{'a1' * 32}` |" in text
    assert f"| `helios-compiler-host` | `{'b2' * 32}` |" in text
    assert "Compiler plugin" not in render_pins(baseline_report)
