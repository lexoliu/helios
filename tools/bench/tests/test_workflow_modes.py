import json
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


CANDIDATE_SHA = "0123456789abcdef0123456789abcdef01234567"
PR_BASE_SHA = "fedcba9876543210fedcba9876543210fedcba98"


@pytest.mark.parametrize(
    (
        "event",
        "requested",
        "baseline",
        "build",
        "pr_bench",
        "tcp_probe",
        "advisory",
        "paired",
        "per_column",
        "baseline_profile",
    ),
    [
        ("workflow_dispatch", "true", "baseline", "profile-use", "", "false", "true", "true", "true", "true"),
        ("workflow_dispatch", "true", "", "profile-use", "", "false", "true", "false", "false", "false"),
        # The plain control (#322) is a pairing of one commit against itself built
        # plain; it collects the candidate's profile and no baseline's.
        ("workflow_dispatch", "true", "", "release", "", "false", "true", "true", "true", "false"),
        # A baseline ref outside acceptance still names a column with a
        # profile to collect; `per_column_profiles` is what keeps a
        # non-paired run from spending it.
        (
            "workflow_dispatch",
            "false",
            "baseline",
            "profile-use",
            "",
            "false",
            "false",
            "false",
            "false",
            "true",
        ),
        ("workflow_dispatch", "false", "", "release", "", "false", "false", "false", "false", "false"),
        ("workflow_dispatch", "false", "", "profile-use", "", "false", "false", "false", "false", "false"),
        # The TCP probe pairs for diagnosis and reads the fetched profile.
        ("workflow_dispatch", "true", "baseline", "profile-use", "", "true", "true", "true", "false", "true"),
        # A labelled pull request is paired against its base SHA and
        # collects a profile for each column (#384); an unlabelled one
        # runs nothing.
        ("pull_request", "", "", "", "true", "", "true", "false", "true", "true"),
        ("pull_request", "", "", "", "false", "", "true", "false", "false", "false"),
        ("push", "", "", "", "", "", "false", "false", "false", "false"),
    ],
)
def test_workflow_mode(
    jobs,
    tmp_path,
    event,
    requested,
    baseline,
    build,
    pr_bench,
    tcp_probe,
    advisory,
    paired,
    per_column,
    baseline_profile,
):
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
            "TCP_PROBE": tcp_probe,
            "PR_BENCH": pr_bench,
            "PR_BASE_SHA": PR_BASE_SHA if event == "pull_request" else "",
            "GITHUB_OUTPUT": str(output),
        },
    )
    outputs = dict(line.split("=", 1) for line in output.read_text().splitlines())
    assert outputs["advisory"] == advisory
    assert outputs["paired_acceptance"] == paired
    assert outputs["per_column_profiles"] == per_column
    assert outputs["baseline_profile"] == baseline_profile
    # The ref the suite pairs against: the dispatch's, or a labelled
    # pull request's base (#384).
    assert outputs["pairing_ref"] == (PR_BASE_SHA if pr_bench == "true" else baseline)


def git(repo: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repo), *arguments], check=True, capture_output=True, text=True
    ).stdout.strip()


@pytest.fixture
def history(tmp_path: Path) -> tuple[Path, str, str]:
    """A repository with a `dev` the candidate branched from: the merge
    base and its short SHA are what the columns step has to resolve."""
    repo = tmp_path / "repo"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "dev")
    git(repo, "config", "user.email", "bench@helios.test")
    git(repo, "config", "user.name", "bench")
    (repo / "a").write_text("base\n")
    git(repo, "add", "a")
    git(repo, "-c", "commit.gpgsign=false", "commit", "-q", "-m", "base")
    base = git(repo, "rev-parse", "HEAD")
    git(repo, "update-ref", "refs/remotes/origin/dev", base)
    git(repo, "checkout", "-q", "-b", "topic")
    (repo / "a").write_text("topic\n")
    git(repo, "-c", "commit.gpgsign=false", "commit", "-q", "-am", "topic")
    return repo, base, git(repo, "rev-parse", "HEAD")


@pytest.mark.parametrize(
    ("baseline_profile", "pairing_ref"),
    [
        ("true", "short"),
        ("true", "merge-base"),
        ("false", ""),
    ],
)
def test_profile_columns_resolve_the_pairing_ref_to_its_commit(
    jobs, history, tmp_path: Path, baseline_profile: str, pairing_ref: str
):
    """#384: `actions/checkout` reads a `ref` that is not forty hex digits
    as a branch or tag — run 34749184432's baseline collection fetched
    `refs/heads/c2acd7a4*` and found nothing — so the matrix carries
    commits, resolved where the history is."""
    repo, base, head = history
    columns = next(step for step in jobs["tooling"]["steps"] if step.get("id") == "columns")
    output = tmp_path / "outputs"
    subprocess.run(
        ["bash", "-eu", "-c", columns["run"]],
        check=True,
        cwd=repo,
        env=os.environ
        | {
            "CANDIDATE_SHA": head,
            "PAIRING_REF": base[:8] if pairing_ref == "short" else pairing_ref,
            "BASELINE_PROFILE": baseline_profile,
            "GITHUB_OUTPUT": str(output),
        },
    )
    outputs = dict(line.split("=", 1) for line in output.read_text().splitlines())
    expected = [{"column": "candidate", "ref": head, "artifact": "helios-kernel-profdata-candidate"}]
    if baseline_profile == "true":
        expected.append({"column": "baseline", "ref": base, "artifact": "helios-kernel-profdata-baseline"})
    assert json.loads(outputs["profile_columns"]) == expected


def run_arguments(
    script: str,
    tmp_path: Path,
    paired: str,
    baseline: str = "baseline",
    baseline_kernel_build: str = "profile-use",
    per_column_profiles: str = "false",
    baseline_profile: str = "false",
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
            "BENCH_PER_COLUMN_PROFILES": per_column_profiles,
            "BENCH_BASELINE_PROFILE": baseline_profile,
            "BENCH_LANE": "x86-64-kvm",
        },
    )
    return result.stdout.splitlines()


@pytest.mark.parametrize("paired", ["true", "false"])
@pytest.mark.parametrize("per_column_profiles", ["true", "false"])
@pytest.mark.parametrize("baseline_profile", ["true", "false"])
def test_suite_preserves_workloads_and_pairing(
    jobs, tmp_path: Path, paired, per_column_profiles, baseline_profile
):
    suite = next(step for step in jobs["suite"]["steps"] if step.get("name") == "Run the suite")
    assert suite["if"] == "${{ !inputs.tcp_probe }}"
    arguments = run_arguments(
        suite["run"],
        tmp_path,
        paired,
        per_column_profiles=per_column_profiles,
        baseline_profile=baseline_profile,
    )
    assert arguments[arguments.index("--baseline-ref") + 1] == "baseline"
    assert arguments[arguments.index("--iterations") + 1] == "11"
    assert "--workload" not in arguments
    assert "--reuse-host-listeners" not in arguments
    if paired == "true":
        assert arguments[arguments.index("--sides") + 1] == "helios,helios_baseline"
    else:
        assert "--sides" not in arguments
    # Each column is built against the profile its own collection produced
    # this run, never the fetched one both would otherwise share (#384) —
    # in acceptance and in a labelled pull request's pairing alike; a
    # plain `release` control has no profile to name.
    if per_column_profiles == "true":
        assert arguments[arguments.index("--profile-use") + 1] == (
            f"{tmp_path}/helios/target/pgo-candidate/helios-kernel.profdata"
        )
        if baseline_profile == "true":
            assert arguments[arguments.index("--baseline-profile-use") + 1] == (
                f"{tmp_path}/helios/target/pgo-baseline/helios-kernel.profdata"
            )
        else:
            assert "--baseline-profile-use" not in arguments
    else:
        assert "--profile-use" not in arguments
        assert "--baseline-profile-use" not in arguments


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
    assert jobs["tooling"]["outputs"]["profile_columns"] == "${{ steps.columns.outputs.profile_columns }}"
    checkout = next(step for step in jobs["tooling"]["steps"] if step.get("uses") == "actions/checkout@v4")
    assert checkout["with"]["fetch-depth"] == "0"
    assert jobs["tooling"]["outputs"]["baseline_profile"] == "${{ steps.mode.outputs.baseline_profile }}"
    assert jobs["suite"]["env"]["BENCH_PAIRED_ACCEPTANCE"] == "${{ needs.tooling.outputs.paired_acceptance }}"
    assert jobs["suite"]["env"]["BENCH_BASELINE_PROFILE"] == "${{ needs.tooling.outputs.baseline_profile }}"
    # One collection per column of a paired run (#384), each checked out
    # at the ref the column carries.
    profile_columns = jobs["profile-columns"]
    assert profile_columns["needs"] == "tooling"
    assert profile_columns["if"] == "needs.tooling.outputs.per_column_profiles == 'true'"
    assert (
        jobs["tooling"]["outputs"]["per_column_profiles"] == "${{ steps.mode.outputs.per_column_profiles }}"
    )
    assert (
        jobs["suite"]["env"]["BENCH_PER_COLUMN_PROFILES"]
        == "${{ needs.tooling.outputs.per_column_profiles }}"
    )
    assert profile_columns["uses"] == "./.github/workflows/kernel-profile.yml"
    assert profile_columns["strategy"]["matrix"]["include"] == (
        "${{ fromJSON(needs.tooling.outputs.profile_columns) }}"
    )
    assert profile_columns["with"] == {
        "ref": "${{ matrix.ref }}",
        "artifact": "${{ matrix.artifact }}",
    }
    # A non-paired run is not held to a skipped collection job; a paired
    # one fails here rather than comparing a column on a stale profile.
    assert jobs["suite"]["needs"] == ["tooling", "profile-columns"]
    assert "always()" in jobs["suite"]["if"]
    assert "needs.profile-columns.result == 'success'" in jobs["suite"]["if"]
    assert "needs.profile-columns.result == 'skipped'" in jobs["suite"]["if"]
    assert jobs["profile-generate"]["needs"] == "tooling"
    assert jobs["profile-generate"]["if"].startswith("needs.tooling.outputs.paired_acceptance != 'true'")
    assert jobs["suite-pgo"]["needs"] == "profile-generate"
    assert jobs["suite-pgo"]["if"] == "needs.profile-generate.result == 'success'"
    assert jobs["gate"]["needs"] == ["tooling", "suite"]


def test_a_paired_run_downloads_each_columns_own_profile(jobs):
    """#384: each column of a paired run builds against the profile
    its own collection produced, named for the column and downloaded under
    the path `run` is told to read."""
    steps = {step.get("name"): step for step in jobs["suite"]["steps"]}
    candidate = steps["Download the candidate column's kernel profile"]
    assert candidate["if"] == "env.BENCH_PER_COLUMN_PROFILES == 'true'"
    assert candidate["uses"] == "actions/download-artifact@v4"
    assert candidate["with"]["name"] == "helios-kernel-profdata-candidate"
    assert candidate["with"]["path"] == "helios/target/pgo-candidate"
    baseline = steps["Download the baseline column's kernel profile"]
    assert "env.BENCH_PER_COLUMN_PROFILES == 'true'" in baseline["if"]
    assert "env.BENCH_BASELINE_PROFILE == 'true'" in baseline["if"]
    assert baseline["with"]["name"] == "helios-kernel-profdata-baseline"
    assert baseline["with"]["path"] == "helios/target/pgo-baseline"


def test_kernel_profile_collects_the_ref_it_is_asked_for():
    """`kernel-profile.yml` is reusable per column: the ref it checks out
    and the artifact it uploads are the caller's."""
    workflow = yaml.load(
        (REPO_ROOT / ".github/workflows/kernel-profile.yml").read_text(), Loader=yaml.BaseLoader
    )
    inputs = workflow["on"]["workflow_call"]["inputs"]
    assert inputs["ref"]["type"] == "string"
    assert inputs["ref"]["default"] == ""
    assert inputs["artifact"]["type"] == "string"
    assert inputs["artifact"]["default"] == "helios-kernel-profdata"
    steps = workflow["jobs"]["kernel-profile"]["steps"]
    checkout = next(step for step in steps if step.get("uses") == "actions/checkout@v4")
    assert checkout["with"]["ref"] == "${{ inputs.ref || github.sha }}"
    upload = next(step for step in steps if step.get("uses") == "actions/upload-artifact@v4")
    assert "inputs.artifact" in upload["with"]["name"]
    # A run nobody called — the schedule, a manual dispatch — keeps
    # publishing the fetched profile under its stable name.
    assert "helios-kernel-profdata" in upload["with"]["name"]


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
