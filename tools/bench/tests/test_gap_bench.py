"""What a lost guest costs the report.

The suite boots one guest per workload class, so a class whose kernel
died must leave a stated failure for every workload it never measured:
a cell that vanishes reads as a workload nobody asked for.
"""

from __future__ import annotations

import json
from pathlib import Path

from helios_bench.wasi_apps import gap_bench

WORKLOADS = [
    {"name": "instance-startup-1", "class": "startup", "headline": False, "runner": "program"},
    {"name": "instance-startup-100", "class": "startup", "headline": True, "runner": "program"},
    {"name": "instance-startup-500", "class": "startup", "headline": False, "runner": "program"},
    {"name": "spawn-wait", "class": "startup", "headline": True, "runner": "program"},
]

SELECTED = [workload["name"] for workload in WORKLOADS]

REASON = "tools/wasi-apps/workload-bench.sh exited with status 1"


def records(log: Path) -> list[dict]:
    return [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines() if line.strip()]


def test_a_lost_class_leaves_a_failure_for_every_unmeasured_workload(tmp_path) -> None:
    log = tmp_path / "helios-startup.jsonl"
    log.write_text(
        "\n".join(
            [
                json.dumps({"type": "run", "schema_version": 1, "selected_workloads": SELECTED}),
                json.dumps(
                    {
                        "type": "summary",
                        "workload": "instance-startup-1",
                        "class": "startup",
                        "median_elapsed_ms": 26,
                    }
                ),
                json.dumps(
                    {
                        "type": "failure",
                        "workload": "instance-startup-100",
                        "class": "startup",
                        "error": "SpawnErrorKind::OutOfMemory",
                    }
                ),
            ]
        )
        + "\n",
        encoding="utf-8",
    )

    gap_bench().record_unmeasured(log, WORKLOADS, SELECTED, REASON)

    written = records(log)
    # The measured workload keeps its number and the recorded failure keeps
    # its own reason: neither is overwritten.
    assert [record["type"] for record in written[:3]] == ["run", "summary", "failure"]
    assert written[2]["error"] == "SpawnErrorKind::OutOfMemory"
    added = {record["workload"]: record for record in written[3:]}
    assert set(added) == {"instance-startup-500", "spawn-wait"}
    assert all(record["type"] == "failure" for record in added.values())
    assert all(record["error"] == REASON for record in added.values())
    assert added["spawn-wait"]["headline"] is True
    assert added["instance-startup-500"]["class"] == "startup"


def test_a_class_that_never_started_leaves_a_failure_for_all_of_it(tmp_path) -> None:
    log = tmp_path / "helios-net.jsonl"

    gap_bench().record_unmeasured(log, WORKLOADS, SELECTED, REASON)

    assert [record["workload"] for record in records(log)] == SELECTED
