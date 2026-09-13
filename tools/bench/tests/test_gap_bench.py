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


def test_the_plain_control_reaches_the_harness_and_the_artifact_lookup(tmp_path):
    """The baseline built without the fetched profile (#322) is asked for
    through the harness environment and identified the same way when the
    inspector is asked which kernel it would boot."""
    module = gap_bench()
    control = module.HeliosImage(
        name="helios-baseline",
        workspace_root=tmp_path,
        out_dir=tmp_path / "out",
        without_kernel_profile=True,
    )
    candidate = module.HeliosImage(name="helios", workspace_root=tmp_path, out_dir=tmp_path / "out")
    assert (
        module.harness_environment(control, paired=True)["HELIOS_WORKLOAD_BENCH_WITHOUT_KERNEL_PROFILE"]
        == "1"
    )
    assert (
        module.harness_environment(candidate, paired=True)["HELIOS_WORKLOAD_BENCH_WITHOUT_KERNEL_PROFILE"]
        == ""
    )


def test_the_baselines_own_profile_reaches_the_harness(tmp_path):
    """#384: the baseline image's own collection is handed to its boots
    the way the candidate's is, and the parser takes the flag the plan
    emits."""
    module = gap_bench()
    profile = tmp_path / "helios-kernel.profdata"
    profile.write_bytes(b"\x00" * 16)
    baseline = module.HeliosImage(
        name="helios-baseline",
        workspace_root=tmp_path / "baseline",
        out_dir=tmp_path / "out",
        profile_use=profile,
    )
    candidate = module.HeliosImage(
        name="helios",
        workspace_root=tmp_path,
        out_dir=tmp_path / "out",
        profile_use=tmp_path / "candidate.profdata",
    )
    assert module.harness_environment(baseline, paired=True)["HELIOS_WORKLOAD_BENCH_PROFILE_USE"] == str(
        profile
    )
    assert module.harness_environment(candidate, paired=True)["HELIOS_WORKLOAD_BENCH_PROFILE_USE"] == str(
        tmp_path / "candidate.profdata"
    )
    args = module.build_parser().parse_args(["--helios-baseline-profile-use", str(profile)])
    assert args.helios_baseline_profile_use == profile
