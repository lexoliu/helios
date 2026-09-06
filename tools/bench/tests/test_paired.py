"""The paired mode: two Helios images, one host, one job.

Run 33990628290 reported every workload 20-40% faster than the `dev` run
it was compared against, including workloads its change could not touch,
because it landed on a faster shared runner (#173). What is under test
here is the answer to that: how the driver orders the boots of the two
images, that a report can carry the second column and be read back, and
that the gate between the two columns enforces where the cross-run one
cannot.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from bench_stub import fake_checkout, records, workload

from helios_bench.baseline import Baseline
from helios_bench.gate import GateKind, evaluate, evaluate_paired, gate_report
from helios_bench.manifest import load_manifest
from helios_bench.plots import plot_report
from helios_bench.render import render_gate, render_tables
from helios_bench.report import Report, Side, load_report, save_report
from helios_bench.runner import GAP_BENCH, RunOptions, plan
from helios_bench.wasi_apps import gap_bench

# Two classes, three workloads, and none of them the class that wedges:
# the order of the boots is what is under test here, not what a lost
# guest costs.
WORKLOADS = [
    workload("quickjs-loop", "compute"),
    workload("cpython-json", "compute"),
    workload("hostcall-loop", "hostcall"),
]


def boot_order(order_log: Path) -> list[tuple[str, str, str]]:
    """(harness, guest workspace, workloads) of every boot, in order."""
    return [
        tuple(line.split(" ", 2))
        for line in order_log.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


@pytest.fixture
def driver(tmp_path, monkeypatch):
    module = gap_bench()
    fake_checkout(tmp_path / "candidate")
    fake_checkout(tmp_path / "baseline")
    monkeypatch.setattr(module, "repo_root", lambda: tmp_path / "candidate")
    # The real one asks this checkout's inspector where the guest kernel
    # is; the stand-in writes one where the query would find it.
    monkeypatch.setattr(
        module,
        "guest_artifact",
        lambda image, arch, accel=None: image.workspace_root / "kernel",
    )
    monkeypatch.setenv("HELIOS_TEST_WEDGE_PID_FILE", str(tmp_path / "wedge.pid"))
    monkeypatch.setenv("HELIOS_TEST_BUILD_LOG", str(tmp_path / "builds"))
    monkeypatch.setenv("HELIOS_TEST_BUILD_SECONDS", "0")
    monkeypatch.setenv("HELIOS_TEST_ORDER", str(tmp_path / "order"))
    return module


def run_side(driver, tmp_path, images: list) -> Path:
    driver.run_helios(
        Path("tools/wasi-apps/workloads.json"),
        images,
        1,
        WORKLOADS,
        "x86-64",
        "kvm",
        None,
        None,
        None,
        None,
        timeout_seconds=60,
        side_timeout_seconds=600,
        build_timeout_seconds=60,
        skip_build=False,
        control_workload=None,
        keep_going=True,
    )
    return tmp_path / "order"


def images_of(driver, tmp_path, paired: bool) -> list:
    images = [
        driver.HeliosImage(
            name="helios",
            workspace_root=tmp_path / "candidate",
            out_dir=tmp_path / "out" / "helios",
        )
    ]
    if paired:
        images.append(
            driver.HeliosImage(
                name="helios-baseline",
                workspace_root=tmp_path / "baseline",
                out_dir=tmp_path / "out" / "helios-baseline",
            )
        )
    for image in images:
        image.out_dir.mkdir(parents=True)
    return images


def test_a_paired_side_boots_the_two_images_back_to_back(driver, tmp_path) -> None:
    """Every workload's two boots are adjacent, and the order alternates.

    Two guest images cannot share a guest, so the boot is the smallest
    unit the pairing has: what it buys is that the candidate's boot of a
    workload follows the baseline's boot of that same workload with
    nothing in between, rather than a whole side later. Which of them
    goes first alternates, so neither systematically holds the earlier
    slot of the pair.
    """
    images = images_of(driver, tmp_path, paired=True)
    order = boot_order(run_side(driver, tmp_path, images))

    assert [names for _, _, names in order] == [
        "quickjs-loop",
        "quickjs-loop",
        "cpython-json",
        "cpython-json",
        "hostcall-loop",
        "hostcall-loop",
    ], "one boot per workload per image, and the pair is never split"
    assert [guest for _, guest, _ in order] == [
        "candidate",
        "baseline",
        "baseline",
        "candidate",
        "candidate",
        "baseline",
    ]
    for image in images:
        measured = records(image.out_dir / "helios.jsonl")
        assert {name: record["type"] for name, record in measured.items()} == {
            "quickjs-loop": "summary",
            "cpython-json": "summary",
            "hostcall-loop": "summary",
        }


def test_an_unpaired_side_still_boots_one_guest_per_class(driver, tmp_path) -> None:
    """The ordinary run is untouched: one guest per class, as published."""
    images = images_of(driver, tmp_path, paired=False)
    order = boot_order(run_side(driver, tmp_path, images))

    assert order == [
        ("candidate", "candidate", "quickjs-loop,cpython-json"),
        ("candidate", "candidate", "hostcall-loop"),
    ]


def test_one_harness_boots_both_images(driver, tmp_path) -> None:
    """The baseline supplies a guest, never a harness.

    Run 33995029872 died the other way round: the baseline checkout's own
    `workload-bench.sh` and its own inspector drove its half, and that
    inspector predated a fix to the host side, so the paired run failed
    on the older harness rather than on anything about the guest.
    """
    order = boot_order(run_side(driver, tmp_path, images_of(driver, tmp_path, paired=True)))

    assert {harness for harness, _, _ in order} == {"candidate"}, "every boot runs this checkout's harness"
    assert {guest for _, guest, _ in order} == {"candidate", "baseline"}


def test_two_images_that_are_one_build_are_refused(driver, tmp_path, monkeypatch) -> None:
    """A shared target directory or workspace root would otherwise time
    one build twice and report the noise between it and itself."""
    monkeypatch.setenv("HELIOS_TEST_KERNEL_CONTENT", "one build, twice")
    with pytest.raises(driver.IdenticalHeliosImages, match="same guest build"):
        run_side(driver, tmp_path, images_of(driver, tmp_path, paired=True))


def test_the_refusal_reads_the_artifacts_not_the_paths(driver, tmp_path) -> None:
    """Two checkouts are not the point; two builds are."""
    images = images_of(driver, tmp_path, paired=True)
    for image in images:
        (image.workspace_root / "kernel").write_text(image.name, encoding="utf-8")

    driver.refuse_identical_images(images, "x86-64")

    (images[1].workspace_root / "kernel").write_text(images[0].name, encoding="utf-8")
    with pytest.raises(driver.IdenticalHeliosImages, match="sha256"):
        driver.refuse_identical_images(images, "x86-64")


def test_the_driver_parses_the_baseline_flags_the_plan_emits(tmp_path) -> None:
    """The plan's argv and the driver's parser are edited together."""
    options = RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path,
        advisory=True,
        sides=frozenset({Side.HELIOS, Side.HELIOS_BASELINE}),
        baseline=Baseline(ref="merge-base", sha="a" * 40, worktree=tmp_path / "worktree"),
    )
    commands = [
        command for command in plan(options, load_manifest(), []) if command.argv[1:2] == [str(GAP_BENCH)]
    ]
    assert len(commands) == 1, "a paired run drives the Helios side once, for both images"
    parsed = gap_bench().build_parser().parse_args(commands[0].argv[2:])
    assert parsed.helios_baseline_root == tmp_path / "worktree"
    assert parsed.helios_baseline_out_dir == tmp_path / "helios-baseline"
    # One budget for the Helios half however many images it times: a
    # paired boot carries one workload where an unpaired one carries a
    # whole class, and the driver shares the budget out as it goes.
    assert parsed.helios_side_timeout_seconds == options.helios_side_timeout_seconds


# A per-unit cap far above any share, so that the share is what binds.
PER_UNIT_CAP = 9000


def granted_budgets(
    driver, tmp_path, monkeypatch, control: dict | None, side: int, build_seconds: int
) -> list[int]:
    """What each boot of a paired side was allowed, in order.

    Recorded at the seam rather than inferred from the clock: the budget
    is the one number the accounting produces, and the run 33997256902
    lost was lost by producing zero for every boot of it.
    """
    granted: list[int] = []

    def record(*args, **kwargs) -> None:
        granted.append(kwargs["timeout_seconds"])

    monkeypatch.setattr(driver, "run_helios_once", record)
    monkeypatch.setenv("HELIOS_TEST_BUILD_SECONDS", str(build_seconds))
    driver.run_helios(
        Path("tools/wasi-apps/workloads.json"),
        images_of(driver, tmp_path, paired=True),
        1,
        WORKLOADS,
        "x86-64",
        "kvm",
        None,
        None,
        None,
        None,
        timeout_seconds=PER_UNIT_CAP,
        side_timeout_seconds=side,
        build_timeout_seconds=120,
        skip_build=False,
        control_workload=control,
        keep_going=True,
    )
    return granted


def test_the_paired_budget_charges_nothing_before_the_first_unit(driver, tmp_path, monkeypatch) -> None:
    """#154 hoisted the build out of the budget; the paired plan must not
    put anything back in front of it.

    Run 33997256902 refused all forty-eight of its boots as over budget
    twenty-six minutes into a three-hour side budget, without booting
    once. Both builds happen before the budget starts, the same way the
    single build does, and nothing else comes off the front of it: the
    two builds below take a fifth of the side between them, and the first
    unit still sees the whole of it.
    """
    side = 60
    granted = granted_budgets(driver, tmp_path, monkeypatch, None, side=side, build_seconds=7)

    boots = driver.side_boots(len(WORKLOADS), 2, None)
    assert boots == len(granted) == 6
    expected = driver.class_budget(side, boots, PER_UNIT_CAP)
    # Only the clock ticking through the assertion itself separates the
    # two; the fourteen seconds of build would separate them by more.
    assert expected - granted[0] <= 1, (
        f"the first unit saw {granted[0]}s of a {side}s side budget, not {expected}s: "
        "the build, or a reserve, is being charged to it"
    )
    assert all(budget > 0 for budget in granted)


def test_the_control_boots_take_a_share_and_not_a_cap(driver, tmp_path, monkeypatch) -> None:
    """Counted, not reserved.

    The control runs before and after are the run's own precondition, so
    they must not be starved — but reserving the per-unit cap for them
    starves everything else instead: two caps are more than a whole side.
    Counting them among the boots gives them a share, and the same
    arithmetic that bounds a unit protects them.
    """
    side = 600
    control = workload("quickjs-loop", "compute")
    granted = granted_budgets(driver, tmp_path, monkeypatch, control, side=side, build_seconds=0)

    boots = driver.side_boots(len(WORKLOADS), 2, control)
    assert boots == len(granted) == 10, "three workloads and a control, before and after, per image"
    expected = driver.class_budget(side, boots, PER_UNIT_CAP)
    assert expected - granted[0] <= 1
    assert all(budget > 0 for budget in granted), (
        "every boot gets a share, the two after-control boots included"
    )
    # Each boot hands back what it did not use, so the share only ever
    # widens: nothing is set aside that a later boot cannot reach.
    assert granted == sorted(granted)


def test_a_baseline_side_survives_the_schema_round_trip(paired_regression_report, tmp_path) -> None:
    path = tmp_path / "report.json"
    save_report(paired_regression_report, path)
    read_back = load_report(path)

    assert read_back.run.paired
    assert read_back.run.baseline_git_sha == paired_regression_report.run.baseline_git_sha
    assert read_back.run.baseline_ref == "merge-base"
    assert Side.HELIOS_BASELINE in read_back.measured_sides()
    assert read_back.table_sides() == [
        Side.HELIOS,
        Side.HELIOS_BASELINE,
        Side.LINUX_WASMTIME,
        Side.LINUX_NATIVE,
    ]
    hostcall = read_back.workload("hostcall-loop")
    assert hostcall.cells[Side.HELIOS_BASELINE].warm.count == 10
    # The baseline image is not a side of the three-way comparison: it is
    # the same kernel measured twice, not another runtime.
    assert {comparison.against for comparison in hostcall.comparisons} == {
        Side.LINUX_WASMTIME,
        Side.LINUX_NATIVE,
    }


def test_an_unpaired_report_keeps_its_three_columns(baseline_report: Report) -> None:
    assert not baseline_report.run.paired
    assert baseline_report.table_sides() == [Side.HELIOS, Side.LINUX_WASMTIME, Side.LINUX_NATIVE]
    assert evaluate_paired(baseline_report) is None


def test_a_paired_regression_blocks_on_a_shared_runner(paired_regression_report: Report) -> None:
    """The whole point: the report is advisory and the gate still blocks.

    Both columns came out of one job on one host, so no change of machine
    can explain the shift, whatever the runner was.
    """
    assert not paired_regression_report.run.publishable
    result = evaluate_paired(paired_regression_report)
    rows = {row.workload: row for row in result.rows}

    assert result.kind is GateKind.PAIRED
    assert result.enforced and result.blocking
    assert rows["hostcall-loop"].regression
    assert rows["hostcall-loop"].shift == pytest.approx(0.5, abs=0.1)
    assert not rows["quickjs-loop"].regression


def test_a_paired_improvement_is_named_and_blocks_nothing(paired_improvement_report: Report) -> None:
    result = evaluate_paired(paired_improvement_report)
    rows = {row.workload: row for row in result.rows}

    assert not result.blocking
    assert rows["hostcall-loop"].improvement
    assert rows["hostcall-loop"].shift == pytest.approx(-0.33, abs=0.05)
    assert [row.workload for row in result.improvements] == ["hostcall-loop"]


def test_a_paired_shift_inside_the_floor_is_neither(paired_flat_report: Report) -> None:
    """A few tenths of a percent is the machine, not the change."""
    result = evaluate_paired(paired_flat_report)
    rows = {row.workload: row for row in result.rows}

    assert result.noise_floor > 0
    assert abs(rows["hostcall-loop"].shift) < result.noise_floor
    assert not rows["hostcall-loop"].beyond_noise
    assert not rows["hostcall-loop"].regression and not rows["hostcall-loop"].improvement
    assert not result.blocking


def test_a_paired_run_that_measured_no_baseline_is_a_failure(paired_regression_report: Report) -> None:
    for result in paired_regression_report.workloads:
        result.cells.pop(Side.HELIOS_BASELINE, None)
    with pytest.raises(SystemExit, match="failed run"):
        evaluate_paired(paired_regression_report)


def test_the_gate_puts_the_paired_table_before_the_cross_run_one(
    paired_regression_report: Report, baseline_report: Report
) -> None:
    report = gate_report(paired_regression_report, baseline_report)
    text = render_gate(report, paired_regression_report.run.lane)

    assert report.paired is not None and report.cross_run is not None
    assert report.cross_run.kind is GateKind.CROSS_RUN
    assert text.index("Paired, one host, one job") < text.index("Cross-run")
    assert report.blocking, "the paired half blocks even though the cross-run half cannot"
    # The cross-run comparison is unchanged: two advisory reports, so it
    # states what it saw and enforces nothing.
    assert not evaluate(baseline_report, paired_regression_report).enforced


def test_a_profile_use_run_pairs_two_builds_of_one_checkout(tmp_path) -> None:
    """The other axis of a pairing: one commit, two builds (#211).

    A PGO candidate has no second checkout to name, so the plan asks the
    driver for the profile instead and the driver pairs it against the
    plain release build of the checkout it lives in. The baseline output
    directory travels either way: the two images write the same file
    names.
    """
    profile = tmp_path / "helios-kernel.profdata"
    profile.write_bytes(b"\x00" * 16)
    options = RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path / "out",
        advisory=True,
        sides=frozenset({Side.HELIOS, Side.HELIOS_BASELINE}),
        profile_use=profile,
    )
    assert options.kernel_build == "profile-use"
    invocations = [
        command.argv
        for command in plan(options, load_manifest(), WORKLOADS)
        if command.argv[1:2] == [str(GAP_BENCH)]
    ]
    assert len(invocations) == 1, "the Helios side alone; no Linux side was asked for"
    args = gap_bench().build_parser().parse_args(invocations[0][2:])
    assert args.helios_profile_use == profile
    assert args.helios_baseline_root is None, "the baseline is this checkout, built plain"
    assert args.helios_baseline_out_dir == options.out_dir / "helios-baseline"


def test_a_pgo_pairing_names_the_build_in_both_columns(paired_regression_report) -> None:
    """Two columns of one commit are told apart by their build.

    The paired table labels each column with its commit, which is the
    only thing that varies when the baseline is another ref. A PGO
    pairing varies the build instead, so both columns would otherwise
    carry the same label and the table would say nothing about which one
    read the profile.
    """
    sha = paired_regression_report.run.helios_git_sha
    run = paired_regression_report.run.model_copy(
        update={
            "baseline_git_sha": sha,
            "baseline_ref": None,
            "kernel_build": "profile-use",
            "baseline_kernel_build": "release",
        }
    )
    result = evaluate_paired(paired_regression_report.model_copy(update={"run": run}))

    assert result is not None and result.kind is GateKind.PAIRED
    assert "release" in result.baseline_label
    assert "profile-use" in result.candidate_label
    assert result.baseline_label != result.candidate_label
    rendered = render_gate(
        gate_report(paired_regression_report.model_copy(update={"run": run}), None), run.lane
    )
    assert "profile-use" in rendered


def test_a_pgo_run_reports_without_a_linux_side(tmp_path) -> None:
    """`suite-pgo` times Helios against Helios and nothing else.

    The three-way comparison answers a different question and would
    double a job that already boots every workload twice, so the PGO job
    runs `--sides helios,helios_baseline`. A report with neither Linux
    column still has to assemble, render and gate.
    """
    from conftest import THRESHOLDS, iterations, raw_side
    from conftest import WORKLOADS as SYNTHETIC

    from helios_bench.assemble import assemble_report, build_control
    from helios_bench.report import Hardware, Pins, RunInfo

    sha = "0123456789abcdef0123456789abcdef01234567"
    centers = {"hostcall-loop": 100.0, "quickjs-loop": 90.0, "fs-smallfiles": 40.0}
    sides = {
        Side.HELIOS: raw_side({name: iterations(value, 1.0, 5) for name, value in centers.items()}),
        Side.HELIOS_BASELINE: raw_side(
            {name: iterations(value * 1.1, 1.0, 6) for name, value in centers.items()}
        ),
    }
    control_sides = {
        side: (
            raw_side({"quickjs-loop": iterations(100.0, 1.0, 7)}),
            raw_side({"quickjs-loop": iterations(101.0, 1.0, 8)}),
        )
        for side in sides
    }
    report = assemble_report(
        SYNTHETIC,
        sides,
        build_control("quickjs-loop", control_sides, THRESHOLDS),
        RunInfo(
            id="4004",
            url=None,
            attempt=1,
            lane="x86-64-kvm",
            runner_label="ubuntu-24.04",
            advisory=True,
            publishable=False,
            deviations=[],
            started_at="2026-09-05T00:00:00+00:00",
            finished_at="2026-09-05T02:00:00+00:00",
            helios_git_sha=sha,
            baseline_git_sha=sha,
            baseline_ref=None,
            kernel_build="profile-use",
            baseline_kernel_build="release",
        ),
        Hardware(
            host_os="Linux 6.11",
            host_arch="x86_64",
            cpu="AMD EPYC 7763",
            logical_cpus=4,
            memory_bytes=16 << 30,
            accelerator="kvm",
            qemu_version="8.2.2",
        ),
        Pins(
            wasmtime_revision="39819b1f81f3912dddfdcb25de6d5924aef15783",
            wasmtime_linux_release="wasmtime-v48.0.0-x86_64-linux",
            fedora_image_url="https://download.fedoraproject.org/example.qcow2",
            fedora_image_sha256="ef" * 32,
            qemu_version="8.2.2",
            vcpus=4,
            memory="6G",
            linux_vm_memory="4G",
            net_backend="tap",
            devices=["virtio-net-pci"],
            wasm_artifacts={},
            bootfs_cwasm={},
        ),
        THRESHOLDS,
    )

    assert set(report.measured_sides()) == {Side.HELIOS, Side.HELIOS_BASELINE}
    # Everything `helios-bench run` writes beside the report has to
    # survive a run with no Linux column, because the job never has one.
    assert "quickjs-loop" in render_tables(report)
    plots = plot_report(report, tmp_path)
    assert plots
    assert all(not path.name.endswith("-headline.svg") for path in plots)
    save_report(report, tmp_path / "report.json")
    result = evaluate_paired(load_report(tmp_path / "report.json"))
    assert result is not None
    assert result.rows, "the PGO candidate is compared against the plain image"
    assert "profile-use" in render_gate(gate_report(report, None), report.run.lane)
