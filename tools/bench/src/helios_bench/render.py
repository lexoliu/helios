"""Markdown rendering from reports, through compiled Jinja templates."""

from __future__ import annotations

import re
from pathlib import Path

from jinja2 import Environment, FileSystemLoader, StrictUndefined

from helios_bench import PACKAGE_ROOT
from helios_bench.gate import GATE_TITLES, GateKind, GateReport
from helios_bench.report import CLASS_LABELS, SIDE_LABELS, Report, Side, WorkloadClass

TEMPLATES_ROOT = PACKAGE_ROOT / "templates"
README_BEGIN = "<!-- helios-bench:begin run={run_id} -->"
README_END = "<!-- helios-bench:end -->"
MARKER_PATTERN = re.compile(
    r"<!-- helios-bench:begin run=(?P<run>[^ ]+) -->\n(?P<body>.*?)\n<!-- helios-bench:end -->", re.S
)


def format_ms(value: float) -> str:
    if value >= 100:
        return f"{value:,.0f}"
    if value >= 10:
        return f"{value:.1f}"
    return f"{value:.2f}"


def format_ci(low: float, high: float) -> str:
    return f"[{format_ms(low)}, {format_ms(high)}]"


def format_speedup(value: float) -> str:
    return f"{value:.2f}x"


def format_percent(value: float) -> str:
    return f"{value * 100:+.1f}%"


def environment() -> Environment:
    env = Environment(
        loader=FileSystemLoader(TEMPLATES_ROOT),
        undefined=StrictUndefined,
        trim_blocks=True,
        lstrip_blocks=True,
        keep_trailing_newline=True,
        autoescape=False,
    )
    env.filters["ms"] = format_ms
    env.filters["ci"] = format_ci
    env.filters["speedup"] = format_speedup
    env.filters["percent"] = format_percent
    env.globals["Side"] = Side
    env.globals["SIDE_LABELS"] = SIDE_LABELS
    env.globals["CLASS_LABELS"] = CLASS_LABELS
    env.globals["WorkloadClass"] = WorkloadClass
    env.globals["GateKind"] = GateKind
    env.globals["GATE_TITLES"] = GATE_TITLES
    return env


def render_tables(report: Report) -> str:
    return environment().get_template("tables.md.j2").render(report=report)


def render_readme_section(reports: list[Report], run_id: str) -> str:
    return environment().get_template("readme.md.j2").render(reports=reports, run_id=run_id)


def render_docs_results(reports: list[Report], run_id: str) -> str:
    return environment().get_template("docs_results.md.j2").render(reports=reports, run_id=run_id)


def render_gate(report: GateReport, lane: str) -> str:
    return environment().get_template("gate.md.j2").render(report=report, lane=lane)


def render_pins(report: Report) -> str:
    return environment().get_template("pins.md.j2").render(report=report)


def replace_marked_section(document: str, run_id: str, body: str) -> str:
    """Replaces the marked section of ``document`` with ``body``.

    The section is delimited by ``README_BEGIN``/``README_END`` markers; the
    begin marker carries the run id every number inside was rendered from.
    """
    replacement = f"{README_BEGIN.format(run_id=run_id)}\n{body.rstrip()}\n{README_END}"
    if MARKER_PATTERN.search(document) is None:
        raise SystemExit("the document has no helios-bench marker block to replace")
    return MARKER_PATTERN.sub(lambda _: replacement, document, count=1)


def marked_section(document: str) -> tuple[str, str]:
    """The run id and body of the marked section of ``document``."""
    match = MARKER_PATTERN.search(document)
    if match is None:
        raise SystemExit("the document has no helios-bench marker block")
    return match.group("run"), match.group("body")


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
