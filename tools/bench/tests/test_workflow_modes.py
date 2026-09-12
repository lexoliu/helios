import os
import re
import subprocess
from pathlib import Path

import pytest
import yaml

from helios_bench.manifest import REPO_ROOT


@pytest.fixture
def workflow():
    path = REPO_ROOT / ".github/workflows/bench-suite.yml"
    return yaml.load(path.read_text(), Loader=yaml.BaseLoader)


@pytest.fixture
def jobs(workflow):
    return workflow["jobs"]


@pytest.mark.parametrize(
    ("event", "requested", "baseline", "build", "advisory", "paired"),
    [
        ("workflow_dispatch", "true", "baseline", "profile-use", "true", "true"),
        ("workflow_dispatch", "true", "", "profile-use", "true", "false"),
        # The plain control (#322) is a pairing of one commit against itself built plain.
        ("workflow_dispatch", "true", "", "release", "true", "true"),
        ("workflow_dispatch", "false", "baseline", "profile-use", "false", "false"),
        ("workflow_dispatch", "false", "", "release", "false", "false"),
        ("workflow_dispatch", "false", "", "profile-use", "false", "false"),
        ("pull_request", "", "", "", "true", "false"),
        ("push", "", "", "", "false", "false"),
    ],
)
def test_workflow_mode(jobs, tmp_path, event, requested, baseline, build, advisory, paired):
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
            "BASELINE_KERNEL_BUILD": build,
            "GITHUB_OUTPUT": str(output),
        },
    )
    assert dict(line.split("=", 1) for line in output.read_text().splitlines()) == {
        "advisory": advisory,
        "paired_acceptance": paired,
    }


def run_arguments(
    script: str,
    tmp_path: Path,
    paired: str,
    baseline: str = "baseline",
    baseline_kernel_build: str = "profile-use",
) -> list[str]:
    expressions = {
        "matrix.lane": "x86-64-kvm",
        "runner.name": "test-runner",
        "inputs.iterations": "11",
        "matrix.net-backend": "user",
        "steps.lane.outputs.net-backend": "user",
    }
    script = re.sub(r"\$\{\{(.*?)\}\}", lambda match: expressions[match[1].strip()], script)
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
            "BENCH_BASELINE_REF": baseline,
            "BENCH_PAIRED_ACCEPTANCE": paired,
            "BENCH_BASELINE_KERNEL_BUILD": baseline_kernel_build,
            "BENCH_LANE": "x86-64-kvm",
        },
    )
    return result.stdout.splitlines()


@pytest.mark.parametrize("paired", ["true", "false"])
def test_suite_preserves_workloads_and_pairing(jobs, tmp_path: Path, paired):
    suite = next(step for step in jobs["suite"]["steps"] if step.get("name") == "Run the suite")
    assert suite["if"] == "${{ !inputs.tcp_probe }}"
    arguments = run_arguments(suite["run"], tmp_path, paired)
    assert arguments[arguments.index("--baseline-ref") + 1] == "baseline"
    assert arguments[arguments.index("--iterations") + 1] == "11"
    assert "--workload" not in arguments
    assert "--reuse-host-listeners" not in arguments
    if paired == "true":
        assert arguments[arguments.index("--sides") + 1] == "helios,helios_baseline"
    else:
        assert "--sides" not in arguments


@pytest.mark.parametrize(
    ("job", "step_name"),
    [
        ("suite", "Run the suite"),
        ("suite-pgo", "Time the profile-guided kernel against the plain one"),
    ],
)
def test_the_retry_budget_is_the_job_timeout(jobs, tmp_path, job, step_name):
    """`--job-timeout-minutes` is the same literal as the job's
    `timeout-minutes`: the one retry of an inconclusive paired control is
    sized against what remains of the job it runs in, so the two cannot
    drift."""
    step = next(step for step in jobs[job]["steps"] if step.get("name") == step_name)
    if job == "suite":
        # The step runs only when tcp_probe is off, which is when the
        # job's timeout expression resolves to its second arm: 420 minutes.
        assert step["if"] == "${{ !inputs.tcp_probe }}"
        assert jobs[job]["timeout-minutes"] == "${{ inputs.tcp_probe && 40 || 420 }}"
    else:
        assert str(jobs[job]["timeout-minutes"]) == "420"
    arguments = run_arguments(step["run"], tmp_path, "true")
    assert arguments[arguments.index("--job-timeout-minutes") + 1] == "420"


@pytest.mark.parametrize(("build", "asked"), [("profile-use", False), ("release", True)])
def test_suite_passes_the_plain_baseline_control_through(jobs, tmp_path: Path, build, asked):
    """`baseline_kernel_build: release` reaches `helios-bench run` as the
    PGO control (#322); the default asks for nothing."""
    suite = next(step for step in jobs["suite"]["steps"] if step.get("name") == "Run the suite")
    assert jobs["suite"]["env"]["BENCH_BASELINE_KERNEL_BUILD"] == "${{ inputs.baseline_kernel_build }}"
    arguments = run_arguments(suite["run"], tmp_path, "true", baseline_kernel_build=build)
    assert ("--baseline-kernel-build" in arguments) is asked
    if asked:
        assert arguments[arguments.index("--baseline-kernel-build") + 1] == "release"


@pytest.mark.parametrize(("probe", "queues"), [("true", "1"), ("false", "8"), ("", "8")])
def test_tap_setup_matches_the_probe_queue_mode(jobs, probe, queues):
    setup = next(
        step for step in jobs["suite"]["steps"] if step.get("name") == "Provision the tap network backend"
    )
    assert setup["env"]["TCP_PROBE"] == "${{ inputs.tcp_probe }}"
    script = setup["run"].replace('"$(nproc)"', '"8"')
    script = script.replace("./target/release/helios-inspector", 'printf "%s\\n"')
    result = subprocess.run(
        ["bash", "-eu", "-c", script],
        check=True,
        capture_output=True,
        text=True,
        env=os.environ
        | {"TCP_PROBE": probe, "HELIOS_NET_IFNAME": "helios0", "HELIOS_NET_BRIDGE": "helios-br0"},
    )
    arguments = result.stdout.splitlines()
    assert arguments[:2] == ["vm", "net-setup"]
    assert arguments[arguments.index("--net-queues") + 1] == queues
    assert "--net-dhcp" in arguments


def test_profile_jobs_follow_the_mode_output(jobs):
    assert jobs["tooling"]["outputs"]["paired_acceptance"] == "${{ steps.mode.outputs.paired_acceptance }}"
    assert jobs["suite"]["env"]["BENCH_PAIRED_ACCEPTANCE"] == "${{ needs.tooling.outputs.paired_acceptance }}"
    assert jobs["profile-generate"]["needs"] == "tooling"
    assert jobs["profile-generate"]["if"].startswith("needs.tooling.outputs.paired_acceptance != 'true'")
    assert jobs["suite-pgo"]["needs"] == "profile-generate"
    assert jobs["suite-pgo"]["if"] == "needs.profile-generate.result == 'success'"
    assert jobs["gate"]["needs"] == ["tooling", "suite"]


def test_keep_going_preserves_failed_workload_logs(jobs):
    upload = next(
        step
        for step in jobs["suite"]["steps"]
        if step.get("name") == "Upload the inspector runtime directory"
    )
    assert upload["if"] == "always()"
    assert "bench-runtime/**/*.log" in upload["with"]["path"].splitlines()


@pytest.mark.parametrize(
    ("filename", "job", "condition"),
    [("ci.yml", "bench", "failure()"), ("bench-suite.yml", "suite", "always()")],
)
def test_benchmark_diagnostics_keep_symbols_but_not_kernels_or_keys(filename, job, condition, tmp_path):
    workflow = yaml.load((REPO_ROOT / ".github/workflows" / filename).read_text(), Loader=yaml.BaseLoader)
    upload = next(
        step
        for step in workflow["jobs"][job]["steps"]
        if step.get("name") == "Upload the inspector runtime directory"
    )
    assert upload["if"] == condition
    paths = upload["with"]["path"].splitlines()
    candidate = Path("helios/target/x86_64-unknown-none/release/helios")
    baseline = Path(
        "helios/target/perf-baselines/worktrees/baseline/target/x86_64-unknown-none/release/helios"
    )
    kernels = [candidate, baseline] if job == "suite" else [candidate]
    snapshots = set()
    for kernel in kernels:
        image = tmp_path / kernel
        image.parent.mkdir(parents=True, exist_ok=True)
        image.write_bytes(b"\x7fELF")
        relative = Path("kernel-symbols") / kernel.with_suffix(".symbols.json")
        snapshot = tmp_path / relative
        snapshot.parent.mkdir(parents=True, exist_ok=True)
        snapshot.write_text("{}")
        snapshots.add(relative)
    key = Path("helios/target/kernel-prebuild/x86_64-unknown-none/release/helios-root-secret.key")
    (tmp_path / key).parent.mkdir(parents=True, exist_ok=True)
    (tmp_path / key).write_bytes(b"test-only-private-key")
    matched = {path.relative_to(tmp_path) for pattern in paths for path in tmp_path.glob(pattern)}
    assert snapshots <= matched
    assert not (set(kernels) | {key}) & matched


def test_ci_preserves_the_built_kernel_before_starting_guests():
    workflow = yaml.load((REPO_ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)
    steps = workflow["jobs"]["bench"]["steps"]
    names = [step.get("name") for step in steps]
    build = names.index("Build the Helios guest and inspector")
    export = names.index("Export benchmark function symbols")
    preserve = names.index("Preserve benchmark function symbols before boot")
    upload = steps[preserve]
    assert build < export < preserve < names.index("Run Helios workload benchmarks")
    assert "if" not in upload
    assert upload["with"]["if-no-files-found"] == "error"
    assert upload["with"]["path"] == "kernel-symbols/**/*.symbols.json"
    assert "helios-bench symbols" in steps[export]["run"]
    assert any(step.get("uses") == "astral-sh/setup-uv@v6" for step in steps[:export])


def test_tcp_probe_is_opt_in_and_not_acceptance(workflow, jobs, tmp_path):
    assert workflow["on"]["workflow_dispatch"]["inputs"]["tcp_probe"]["default"] == "false"
    probe = next(step for step in jobs["suite"]["steps"] if step.get("name") == "Run TCP reconnect probe")
    assert probe["if"] == "inputs.tcp_probe"
    assert probe["env"]["HELIOS_WORKLOAD_BENCH_NET_PCAP"] == "1"
    assert "!inputs.tcp_probe" in jobs["gate"]["if"]
    assert "!inputs.tcp_probe" in jobs["profile-generate"]["if"]
    arguments = run_arguments(probe["run"], tmp_path, "true")
    assert arguments[arguments.index("--workload") + 1] == "tcp-throughput"
    assert arguments[arguments.index("--iterations") + 1] == "2"
    assert arguments[arguments.index("--net-queues") + 1] == "1"
    assert "--reuse-host-listeners" in arguments
    assert arguments[arguments.index("--baseline-ref") + 1] == "baseline"
    assert arguments[arguments.index("--sides") + 1] == "helios,helios_baseline"
    assert "--advisory" in arguments
    upload = next(
        step
        for step in jobs["suite"]["steps"]
        if step.get("name") == "Upload the inspector runtime directory"
    )
    assert "bench-runtime/**/*.pcap" in upload["with"]["path"].splitlines()


def test_tcp_probe_requires_a_baseline(jobs, tmp_path):
    probe = next(step for step in jobs["suite"]["steps"] if step.get("name") == "Run TCP reconnect probe")
    with pytest.raises(subprocess.CalledProcessError) as error:
        run_arguments(probe["run"], tmp_path, "true", baseline="")
    assert "tcp_probe requires baseline_ref" in error.value.stdout


@pytest.mark.parametrize("gate_status", [0, 1, 2])
def test_gate_pipeline_preserves_the_producer_exit_status(jobs, tmp_path, gate_status):
    compare = next(step for step in jobs["gate"]["steps"] if step.get("name") == "Compare")
    script = compare["run"].replace("uv sync --quiet", ":")
    script = script.replace("${{ steps.baseline.outputs.run_id }}", "")
    producer = 'uv run helios-bench "${args[@]}"'
    assert script.count(producer) == 1
    script = script.replace(producer, f'(printf "gate verdict\\n"; exit {gate_status})')
    shell = (
        ["bash", "--noprofile", "--norc", "-eo", "pipefail"]
        if compare.get("shell") == "bash"
        else ["bash", "-e"]
    )
    summary = tmp_path / "summary"
    result = subprocess.run(
        [*shell, "-c", script],
        capture_output=True,
        text=True,
        env=os.environ | {"GITHUB_WORKSPACE": str(tmp_path), "GITHUB_STEP_SUMMARY": str(summary)},
    )
    assert summary.read_text() == "gate verdict\n"
    assert result.returncode == gate_status


def test_vsock_setup_waits_for_device_rules_before_setting_permissions():
    workflow = yaml.load((REPO_ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)
    steps = workflow["jobs"]["smoke-riscv64"]["steps"]
    script = next(
        step["run"]
        for step in steps
        if step.get("name") == "Boot riscv64 guest with the inspector RPC on vsock"
    )
    assert script.index("sudo modprobe vhost_vsock") < script.index("sudo udevadm settle")
    assert script.index("sudo udevadm settle") < script.index("sudo chmod 0666 /dev/vhost-vsock")
    assert script.index("sudo chmod 0666 /dev/vhost-vsock") < script.index(
        "./target/release/helios-inspector"
    )
