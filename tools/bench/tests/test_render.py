from pathlib import Path

import pytest

from helios_bench.gate import GateReport, evaluate
from helios_bench.plots import plot_report
from helios_bench.render import (
    marked_section,
    render_docs_results,
    render_gate,
    render_pins,
    render_readme_section,
    render_tables,
    replace_marked_section,
)
from helios_bench.report import Report


def test_tables_list_every_class_and_flag_parity_bugs(
    baseline_report: Report, advisory_report: Report
) -> None:
    text = render_tables(baseline_report)
    assert "## Host call vs syscall" in text
    assert "## Compute parity" in text
    assert "`hostcall-loop` (headline)" in text
    assert "Noise floor from `quickjs-loop`" in text
    assert "Not publishable" not in text
    advisory = render_tables(advisory_report)
    assert "**parity bug**" in advisory
    assert "Not publishable: advisory run on a shared runner; qemu-system-aarch64 is 10.0.0" in advisory


def test_readme_section_names_the_run_and_only_headline_rows(baseline_report: Report) -> None:
    text = render_readme_section([baseline_report], "1001")
    assert "CI run [1001](https://github.com/lexoliu/helios/actions/runs/1001)" in text
    assert "`hostcall-loop`" in text
    assert "`fs-smallfiles`" not in text
    assert "advisory mode" not in text


def test_readme_section_says_when_numbers_are_advisory(advisory_report: Report) -> None:
    text = render_readme_section([advisory_report], "1003")
    assert "shared runners in advisory mode" in text


def test_marked_section_replacement_round_trip() -> None:
    document = "# Title\n\n<!-- helios-bench:begin run=1 -->\nold\n<!-- helios-bench:end -->\n\ntail\n"
    updated = replace_marked_section(document, "1001", "new body\n")
    assert "run=1001" in updated and "old" not in updated and updated.endswith("tail\n")
    run_id, body = marked_section(updated)
    assert run_id == "1001" and body == "new body"
    with pytest.raises(SystemExit):
        replace_marked_section("no markers", "1", "x")


def test_docs_gate_and_pins_render(baseline_report: Report, regressed_report: Report) -> None:
    docs = render_docs_results([baseline_report], "1001")
    assert "### Lane `x86-64-kvm`" in docs
    assert "benchmarks/runs/1001/x86-64-kvm-hostcall.svg" in docs
    gate = render_gate(
        GateReport(paired=None, cross_run=evaluate(baseline_report, regressed_report)),
        baseline_report.run.lane,
    )
    assert "**regression**" in gate and "**Blocking**" in gate
    pins = render_pins(baseline_report)
    assert "b83d18c8558b6d32fb0c0727d1c6a32639842c49" in pins


def test_plots_write_one_svg_per_class_and_the_headline(tmp_path: Path, baseline_report: Report) -> None:
    written = plot_report(baseline_report, tmp_path)
    names = sorted(path.name for path in written)
    assert names == [
        "x86-64-kvm-compute.svg",
        "x86-64-kvm-fs.svg",
        "x86-64-kvm-headline.svg",
        "x86-64-kvm-hostcall.svg",
    ]
    assert all(path.read_text(encoding="utf-8").lstrip().startswith("<?xml") for path in written)
