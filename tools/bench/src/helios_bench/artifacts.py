"""Committed copies of CI reports and their retrieval from workflow runs.

A README or docs number is traceable to exactly one CI run: the report it
was rendered from is committed under ``docs/benchmarks/runs/<run id>/``
next to the rendered text, and the rendering commands refuse a report
whose own run id differs from the one asked for.
"""

from __future__ import annotations

import shutil
import subprocess
import tempfile
from pathlib import Path

from helios_bench import REPO_ROOT
from helios_bench.report import Report, load_report

RUNS_DIR = REPO_ROOT / "docs" / "benchmarks" / "runs"
ARTIFACT_PREFIX = "bench-report-"
REPORT_FILE = "report.json"


def run_dir(run_id: str, runs_dir: Path = RUNS_DIR) -> Path:
    return runs_dir / run_id


def committed_reports(run_id: str, runs_dir: Path = RUNS_DIR) -> list[Report]:
    directory = run_dir(run_id, runs_dir)
    paths = sorted(directory.glob(f"*/{REPORT_FILE}"))
    if not paths:
        raise SystemExit(
            f"no committed reports under {directory}; fetch them with `helios-bench fetch --run {run_id}`"
        )
    reports = []
    for path in paths:
        report = load_report(path)
        if report.run.id != run_id:
            raise SystemExit(f"{path} was produced by run {report.run.id}, not {run_id}")
        reports.append(report)
    return reports


def fetch_reports(run_id: str, runs_dir: Path = RUNS_DIR, repository: str | None = None) -> list[Path]:
    """Downloads every ``bench-report-<lane>`` artifact of a workflow run."""
    with tempfile.TemporaryDirectory(prefix="helios-bench-fetch-") as scratch:
        command = ["gh", "run", "download", run_id, "--pattern", f"{ARTIFACT_PREFIX}*", "--dir", scratch]
        if repository:
            command.extend(["--repo", repository])
        subprocess.run(command, check=True)
        written = []
        for artifact_dir in sorted(Path(scratch).iterdir()):
            if not artifact_dir.name.startswith(ARTIFACT_PREFIX):
                continue
            lane = artifact_dir.name[len(ARTIFACT_PREFIX) :]
            source = artifact_dir / REPORT_FILE
            if not source.is_file():
                raise SystemExit(f"artifact {artifact_dir.name} carries no {REPORT_FILE}")
            report = load_report(source)
            if report.run.id != run_id:
                raise SystemExit(
                    f"artifact {artifact_dir.name} was produced by run {report.run.id}, not {run_id}"
                )
            destination = run_dir(run_id, runs_dir) / lane / REPORT_FILE
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
            written.append(destination)
    if not written:
        raise SystemExit(f"run {run_id} published no {ARTIFACT_PREFIX}* artifact")
    return written
