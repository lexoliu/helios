import os
import re
import subprocess
from pathlib import Path

import pytest
import yaml

from helios_bench.manifest import REPO_ROOT


@pytest.fixture
def jobs():
    workflow = REPO_ROOT / ".github/workflows/bench-suite.yml"
    return yaml.safe_load(workflow.read_text())["jobs"]


@pytest.mark.parametrize(
    ("event", "requested", "baseline", "advisory", "paired"),
    [
        ("workflow_dispatch", "true", "baseline", "true", "true"),
        ("workflow_dispatch", "true", "", "true", "false"),
        ("workflow_dispatch", "false", "baseline", "false", "false"),
        ("workflow_dispatch", "false", "", "false", "false"),
        ("pull_request", "", "", "true", "false"),
        ("push", "", "", "false", "false"),
    ],
)
def test_workflow_mode(jobs, tmp_path, event, requested, baseline, advisory, paired):
    mode = next(step for step in jobs["tooling"]["steps"] if step.get("id") == "mode")
    output = tmp_path / "outputs"
    subprocess.run(
        ["bash", "-eu", "-c", mode["run"]],
        check=True,
        env=os.environ
        | {
            "EVENT_NAME": event,
            "REQUESTED_ADVISORY": requested,
            "BASELINE_REF": baseline,
            "GITHUB_OUTPUT": str(output),
        },
    )
    assert dict(line.split("=", 1) for line in output.read_text().splitlines()) == {
        "advisory": advisory,
        "paired_acceptance": paired,
    }


@pytest.mark.parametrize("paired", ["true", "false"])
def test_suite_preserves_workloads_and_pairing(jobs, tmp_path: Path, paired):
    suite = next(step for step in jobs["suite"]["steps"] if step.get("name") == "Run the suite")
    expressions = {
        "matrix.lane": "x86-64-kvm",
        "runner.name": "test-runner",
        "inputs.iterations": "11",
        "matrix.net-backend": "user",
    }
    script = re.sub(r"\$\{\{(.*?)\}\}", lambda match: expressions[match[1].strip()], suite["run"])
    invocation = 'uv run helios-bench "${args[@]}"'
    assert script.count(invocation) == 1
    script = script.replace(invocation, 'printf "%s\\n" "${args[@]}"')
    result = subprocess.run(
        ["bash", "-eu", "-c", script],
        check=True,
        capture_output=True,
        text=True,
        env=os.environ
        | {
            "GITHUB_WORKSPACE": str(tmp_path),
            "BENCH_ADVISORY": "true",
            "BENCH_BASELINE_REF": "baseline",
            "BENCH_PAIRED_ACCEPTANCE": paired,
        },
    )
    arguments = result.stdout.splitlines()
    assert arguments[arguments.index("--baseline-ref") + 1] == "baseline"
    assert arguments[arguments.index("--iterations") + 1] == "11"
    assert "--workload" not in arguments
    if paired == "true":
        assert arguments[arguments.index("--sides") + 1] == "helios,helios_baseline"
    else:
        assert "--sides" not in arguments


def test_profile_jobs_follow_the_mode_output(jobs):
    assert jobs["tooling"]["outputs"]["paired_acceptance"] == "${{ steps.mode.outputs.paired_acceptance }}"
    assert jobs["suite"]["env"]["BENCH_PAIRED_ACCEPTANCE"] == "${{ needs.tooling.outputs.paired_acceptance }}"
    assert jobs["profile-generate"]["needs"] == "tooling"
    assert jobs["profile-generate"]["if"].startswith("needs.tooling.outputs.paired_acceptance != 'true'")
    assert jobs["suite-pgo"]["needs"] == "profile-generate"
    assert jobs["suite-pgo"]["if"] == "needs.profile-generate.result == 'success'"
    assert jobs["gate"]["needs"] == ["tooling", "suite"]
