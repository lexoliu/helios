"""Command-line entry point: ``helios-bench <command>``."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from helios_bench import REPO_ROOT
from helios_bench.artifacts import RUNS_DIR, committed_reports, fetch_reports, run_dir
from helios_bench.gate import evaluate
from helios_bench.manifest import host_deviations, load_manifest
from helios_bench.plots import plot_report
from helios_bench.render import (
    marked_section,
    render_docs_results,
    render_gate,
    render_readme_section,
    render_tables,
    replace_marked_section,
    write_text,
)
from helios_bench.report import Side, load_report, save_report
from helios_bench.runner import NetworkOptions, RunOptions, run_suite

README_PATH = REPO_ROOT / "README.md"
DOCS_PATH = REPO_ROOT / "docs" / "benchmarks.md"
# Marker run id of a section that no run has been rendered into yet.
PENDING_RUN = "pending"


def parse_sides(raw: str) -> frozenset[Side]:
    sides = frozenset(Side(item.strip()) for item in raw.split(",") if item.strip())
    if not sides:
        raise SystemExit("--sides names no side")
    return sides


def command_run(args: argparse.Namespace) -> int:
    manifest = load_manifest()
    lane = manifest.lane(args.lane)
    options = RunOptions(
        lane=lane,
        out_dir=args.out_dir.resolve(),
        advisory=args.advisory,
        sides=parse_sides(args.sides),
        workload_names=args.workloads,
        iterations=args.iterations,
        runner_label=args.runner_label,
        allow_busy_host=args.allow_busy_host,
        helios_timeout_seconds=args.helios_timeout_seconds,
        helios_side_timeout_seconds=args.helios_side_timeout_seconds,
        skip_linux_workloads=tuple(args.skip_linux_workloads),
        linux_setup_timeout_seconds=args.linux_setup_timeout_seconds,
        network=NetworkOptions(ifname=args.net_ifname, bridge=args.net_bridge, queues=args.net_queues),
    )
    report = run_suite(options, manifest, dry_run=args.dry_run)
    if report is None:
        return 0
    report_path = options.out_dir / "report.json"
    save_report(report, report_path)
    write_text(options.out_dir / "tables.md", render_tables(report))
    plot_report(report, options.out_dir)
    print(report_path)
    return 0


def command_lanes(args: argparse.Namespace) -> int:
    manifest = load_manifest()
    lanes = manifest.lanes if args.select == "all" else [manifest.lane(args.select)]
    if args.format == "github-matrix":
        print(json.dumps([lane.github_matrix_entry() for lane in lanes]))
    else:
        for lane in lanes:
            print(lane.name)
    return 0


def command_host_check(args: argparse.Namespace) -> int:
    lane = load_manifest().lane(args.lane)
    deviations = host_deviations(lane)
    for deviation in deviations:
        print(f"deviation: {deviation}")
    if not deviations:
        print(f"host matches lane {lane.name}")
    return 1 if deviations else 0


def command_render_tables(args: argparse.Namespace) -> int:
    text = render_tables(load_report(args.report))
    if args.out:
        write_text(args.out, text)
    else:
        sys.stdout.write(text)
    return 0


def command_render_plots(args: argparse.Namespace) -> int:
    for path in plot_report(load_report(args.report), args.out_dir):
        print(path)
    return 0


def readme_text(run_id: str, runs_dir: Path) -> str:
    return render_readme_section(committed_reports(run_id, runs_dir), run_id)


def docs_text(run_id: str, runs_dir: Path) -> str:
    return render_docs_results(committed_reports(run_id, runs_dir), run_id)


def command_render_readme(args: argparse.Namespace) -> int:
    document = args.readme.read_text(encoding="utf-8")
    updated = replace_marked_section(document, args.run, readme_text(args.run, args.runs_dir))
    args.readme.write_text(updated, encoding="utf-8")
    print(f"rendered README performance section from run {args.run}")
    return 0


def command_render_docs(args: argparse.Namespace) -> int:
    reports = committed_reports(args.run, args.runs_dir)
    for report in reports:
        plot_report(report, run_dir(args.run, args.runs_dir))
    document = args.doc.read_text(encoding="utf-8")
    updated = replace_marked_section(document, args.run, render_docs_results(reports, args.run))
    args.doc.write_text(updated, encoding="utf-8")
    print(f"rendered {args.doc} results from run {args.run}")
    return 0


def command_render_check(args: argparse.Namespace) -> int:
    """Fails when a committed rendered section no longer matches the
    committed report of the run its marker names."""
    failures = []
    for path, renderer in ((args.readme, readme_text), (args.doc, docs_text)):
        marker_run, body = marked_section(path.read_text(encoding="utf-8"))
        if args.run and marker_run != args.run:
            failures.append(f"{path}: marker names run {marker_run}, expected {args.run}")
            continue
        if marker_run == PENDING_RUN:
            print(f"{path}: no run rendered yet (marker run={PENDING_RUN})")
            continue
        expected = renderer(marker_run, args.runs_dir).rstrip()
        if body.rstrip() != expected:
            failures.append(f"{path}: rendered section differs from run {marker_run}'s report; re-run render")
    for failure in failures:
        print(failure, file=sys.stderr)
    if not failures:
        print("rendered benchmark sections match their reports")
    return 1 if failures else 0


def command_fetch(args: argparse.Namespace) -> int:
    for path in fetch_reports(args.run, args.runs_dir, args.repo):
        print(path)
    return 0


def command_gate(args: argparse.Namespace) -> int:
    result = evaluate(load_report(args.baseline), load_report(args.candidate))
    text = render_gate(result)
    if args.out:
        write_text(args.out, text)
    sys.stdout.write(text)
    return 1 if result.blocking else 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="helios-bench", description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)

    run = subcommands.add_parser("run", help="time every side on one lane and write report.json")
    run.add_argument("--lane", required=True)
    run.add_argument("--out-dir", type=Path, required=True)
    run.add_argument(
        "--advisory", action="store_true", help="mark the report non-publishable (shared runner)"
    )
    run.add_argument("--dry-run", action="store_true", help="print the plan and host deviations, run nothing")
    run.add_argument("--sides", default="helios,linux_native,linux_wasmtime")
    run.add_argument("--workload", dest="workloads", action="append", default=[])
    run.add_argument("--iterations", type=int)
    run.add_argument("--runner-label")
    run.add_argument("--allow-busy-host", action="store_true")
    run.add_argument("--helios-timeout-seconds", type=int, default=9000)
    run.add_argument("--helios-side-timeout-seconds", type=int, default=10800)
    run.add_argument(
        "--skip-linux-workload",
        dest="skip_linux_workloads",
        action="append",
        default=[],
        help="leave a workload out of the Linux side, by name",
    )
    run.add_argument("--linux-setup-timeout-seconds", type=int, default=5400)
    run.add_argument("--net-ifname")
    run.add_argument("--net-bridge")
    run.add_argument("--net-queues", type=int)
    run.set_defaults(func=command_run)

    lanes = subcommands.add_parser("lanes", help="list the lanes of the manifest")
    lanes.add_argument("--select", default="all", help="a lane name, or all")
    lanes.add_argument("--format", choices=["names", "github-matrix"], default="names")
    lanes.set_defaults(func=command_lanes)

    host_check = subcommands.add_parser("host-check", help="list how this host deviates from a lane")
    host_check.add_argument("--lane", required=True)
    host_check.set_defaults(func=command_host_check)

    render = subcommands.add_parser("render", help="render tables, plots and documentation sections")
    render_commands = render.add_subparsers(dest="render_command", required=True)

    tables = render_commands.add_parser("tables")
    tables.add_argument("--report", type=Path, required=True)
    tables.add_argument("--out", type=Path)
    tables.set_defaults(func=command_render_tables)

    plots = render_commands.add_parser("plots")
    plots.add_argument("--report", type=Path, required=True)
    plots.add_argument("--out-dir", type=Path, required=True)
    plots.set_defaults(func=command_render_plots)

    readme = render_commands.add_parser("readme", help="rewrite the README performance section from a run")
    readme.add_argument("--run", required=True, help="CI run id whose committed reports are rendered")
    readme.add_argument("--runs-dir", type=Path, default=RUNS_DIR)
    readme.add_argument("--readme", type=Path, default=README_PATH)
    readme.set_defaults(func=command_render_readme)

    docs = render_commands.add_parser("docs", help="rewrite the docs/benchmarks.md results from a run")
    docs.add_argument("--run", required=True)
    docs.add_argument("--runs-dir", type=Path, default=RUNS_DIR)
    docs.add_argument("--doc", type=Path, default=DOCS_PATH)
    docs.set_defaults(func=command_render_docs)

    check = render_commands.add_parser("check", help="verify committed sections match their run's report")
    check.add_argument("--run", help="run id the markers must name")
    check.add_argument("--runs-dir", type=Path, default=RUNS_DIR)
    check.add_argument("--readme", type=Path, default=README_PATH)
    check.add_argument("--doc", type=Path, default=DOCS_PATH)
    check.set_defaults(func=command_render_check)

    fetch = subcommands.add_parser(
        "fetch", help="download a run's report artifacts into docs/benchmarks/runs"
    )
    fetch.add_argument("--run", required=True)
    fetch.add_argument("--runs-dir", type=Path, default=RUNS_DIR)
    fetch.add_argument("--repo")
    fetch.set_defaults(func=command_fetch)

    gate = subcommands.add_parser("gate", help="compare a candidate report against a baseline")
    gate.add_argument("--baseline", type=Path, required=True)
    gate.add_argument("--candidate", type=Path, required=True)
    gate.add_argument("--out", type=Path)
    gate.set_defaults(func=command_gate)
    return parser


def main(argv: list[str] | None = None) -> None:
    args = build_parser().parse_args(argv)
    raise SystemExit(args.func(args))
