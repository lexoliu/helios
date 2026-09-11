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

import os
import socket
import subprocess
from contextlib import ExitStack, nullcontext
from pathlib import Path

import pytest
from bench_stub import fake_checkout, fake_inspector, records, workload

from helios_bench.baseline import Baseline
from helios_bench.gate import GateKind, evaluate, evaluate_paired, gate_report
from helios_bench.manifest import load_manifest
from helios_bench.plots import plot_report
from helios_bench.render import render_gate, render_pins, render_tables
from helios_bench.report import Report, Side, load_report, save_report
from helios_bench.runner import (
    GAP_BENCH,
    NetworkOptions,
    RunOptions,
    kernel_pgo_uncovered,
    plan,
    run_suite,
)
from helios_bench.wasi_apps import gap_bench

# Two classes, three workloads, and none of them the class that wedges:
# the order of the boots is what is under test here, not what a lost
# guest costs.
WORKLOADS = [
    workload("quickjs-loop", "compute"),
    workload("cpython-json", "compute"),
    workload("hostcall-loop", "hostcall"),
]


def boot_order(order_log: Path) -> list[tuple[str, str, str, str]]:
    """(harness, guest workspace, inspector, workloads) of every boot, in order."""
    return [
        tuple(line.split(" ", 3))
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


def run_side(driver, tmp_path, images: list, services=None, shared_endpoints=None) -> Path:
    driver.run_helios(
        Path("tools/wasi-apps/workloads.json"),
        images,
        1,
        WORKLOADS,
        "x86-64",
        "kvm",
        services or driver.HostServices(tmp_path, "10.77.0.1", "10.77.0.1"),
        shared_endpoints=shared_endpoints,
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


@pytest.fixture
def host_services(driver, tmp_path, monkeypatch):
    events = []
    ports = iter(range(30000, 30100))

    class Server:
        def __init__(self, port):
            self.port = port

        def shutdown(self):
            events.append(("shutdown", self.port))

        def server_close(self):
            events.append(("close", self.port))

    def start(*args):
        port = next(ports)
        events.append(("start", port))
        return Server(port), port

    monkeypatch.setattr(driver, "start_host_http", start)
    monkeypatch.setattr(driver, "start_tcp_throughput_server", start)
    monkeypatch.setattr(driver, "start_host_tcp_echo", start)
    services = driver.HostServices(tmp_path, "10.77.0.1", "10.77.0.1", http=True, tcp=True, tcp_echo=True)
    return services, events


@pytest.mark.parametrize("reuse", [False, True])
def test_guest_boots_isolate_listeners_unless_reuse_is_explicit(
    driver, tmp_path, monkeypatch, host_services, reuse
):
    services, events = host_services
    endpoints = []
    run_once = driver.run_helios_once

    def record(*args, **kwargs):
        endpoints.append(args[8:12])
        return run_once(*args, **kwargs)

    monkeypatch.setattr(driver, "run_helios_once", record)
    with services.serve() if reuse else nullcontext(None) as shared:
        run_side(driver, tmp_path, images_of(driver, tmp_path, paired=True), services, shared)
    assert len(endpoints) == 6
    expected = 1 if reuse else 6
    assert len({endpoint[0] for endpoint in endpoints}) == expected
    assert len({endpoint[2] for endpoint in endpoints}) == expected
    assert len({endpoint[3] for endpoint in endpoints}) == expected
    assert all(endpoint[1] == "10.77.0.1" for endpoint in endpoints)
    started = [port for event, port in events if event == "start"]
    assert len(started) == expected * 3
    for port in started:
        assert [event for event, value in events if value == port] == ["start", "shutdown", "close"]


def test_host_listeners_close_when_the_guest_scope_fails(host_services):
    services, events = host_services
    with pytest.raises(RuntimeError, match="guest failed"):
        with services.serve():
            raise RuntimeError("guest failed")
    assert [event for event, _ in events].count("close") == 3


def test_host_listeners_close_after_partial_startup(driver, monkeypatch, host_services):
    services, events = host_services

    def refuse(*args):
        raise RuntimeError("bind failed")

    monkeypatch.setattr(driver, "start_tcp_throughput_server", refuse)
    with pytest.raises(RuntimeError, match="bind failed"):
        with services.serve():
            pytest.fail("startup should fail")
    assert [event for event, _ in events] == ["start", "shutdown", "close"]


def test_real_listener_scopes_serve_independent_endpoints(driver, tmp_path, monkeypatch):
    monkeypatch.setattr(driver, "HOST_SERVER_BIND_ADDRESS", "127.0.0.1")
    services = driver.HostServices(tmp_path, "127.0.0.1", "127.0.0.1", tcp_echo=True)
    with services.serve() as first, services.serve() as second:
        assert first.tcp_echo_port != second.tcp_echo_port
        for endpoints in [first, second]:
            with socket.create_connection((endpoints.tcp_host, endpoints.tcp_echo_port), timeout=2) as client:
                client.sendall(b"x")
                assert client.recv(1) == b"x"


def test_guest_scope_closes_accepted_connections(driver, tmp_path, monkeypatch):
    monkeypatch.setattr(driver, "HOST_SERVER_BIND_ADDRESS", "127.0.0.1")
    services = driver.HostServices(tmp_path, "127.0.0.1", "127.0.0.1", tcp_echo=True)
    with ExitStack() as clients:
        with services.serve() as endpoints:
            client = clients.enter_context(
                socket.create_connection((endpoints.tcp_host, endpoints.tcp_echo_port), timeout=2)
            )
            client.sendall(b"x")
            assert client.recv(1) == b"x"
        assert client.recv(1) == b""


def test_shared_listener_diagnosis_cannot_be_published(tmp_path, monkeypatch):
    monkeypatch.setattr("helios_bench.runner.host_deviations", lambda lane: [])
    manifest = load_manifest()
    options = RunOptions(
        lane=manifest.lane("x86-64-kvm"),
        out_dir=tmp_path,
        advisory=False,
        sides=frozenset({Side.HELIOS}),
        network=NetworkOptions(reuse_host_listeners=True),
    )
    with pytest.raises(SystemExit, match="shared host listeners requested"):
        run_suite(options, manifest, dry_run=True)


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

    assert [names for _, _, _, names in order] == [
        "quickjs-loop",
        "quickjs-loop",
        "cpython-json",
        "cpython-json",
        "hostcall-loop",
        "hostcall-loop",
    ], "one boot per workload per image, and the pair is never split"
    assert [guest for _, guest, _, _ in order] == [
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
        ("candidate", "candidate", "-", "quickjs-loop,cpython-json"),
        ("candidate", "candidate", "-", "hostcall-loop"),
    ]


def test_one_harness_boots_both_images(driver, tmp_path) -> None:
    """The baseline supplies a guest, never a harness.

    Run 33995029872 died the other way round: the baseline checkout's own
    `workload-bench.sh` and its own inspector drove its half, and that
    inspector predated a fix to the host side, so the paired run failed
    on the older harness rather than on anything about the guest. What is
    shared is the scheduling harness; the protocol peer is each image's
    own — the next test pins that half.
    """
    order = boot_order(run_side(driver, tmp_path, images_of(driver, tmp_path, paired=True)))

    assert {harness for harness, _, _, _ in order} == {"candidate"}, "every boot runs this checkout's harness"
    assert {guest for _, guest, _, _ in order} == {"candidate", "baseline"}


def test_each_image_is_booted_by_the_tooling_of_its_own_checkout(driver, tmp_path) -> None:
    """#356: the candidate's inspector cannot answer for a baseline guest.

    The inspector and the guest's debugger speak
    helios-inspector-protocol, and a record added between the two refs
    (the `audio` list #347 added to `stats.wit`) left the candidate's
    decoder asking the baseline guest for a field it never sent: run
    34551261487's baseline readiness probe died `DeserializeUnexpectedEnd`.
    Each image is built and booted by the `helios-inspector`/`helios-cli`
    of its own checkout, kept under its own `target/release`.
    """
    order = boot_order(run_side(driver, tmp_path, images_of(driver, tmp_path, paired=True)))

    expected = {
        "candidate": str(tmp_path / "candidate" / "target" / "release" / "helios-inspector"),
        "baseline": str(tmp_path / "baseline" / "target" / "release" / "helios-inspector"),
    }
    for _harness, guest, inspector, _names in order:
        assert inspector == expected[guest]


def test_a_baseline_boot_without_its_own_inspector_is_refused(driver, tmp_path, monkeypatch) -> None:
    """No fallback: a baseline with no inspector of its own never boots.

    The refusal names the path the image's own checkout would have
    produced, and it fires before the first boot rather than inside one —
    and it can never be answered by the candidate's inspector, which is
    the skew of #356.
    """
    inspector = tmp_path / "baseline" / "target" / "release" / "helios-inspector"
    monkeypatch.setenv("HELIOS_TEST_TOOLS_ABSENT", str(inspector))
    with pytest.raises(driver.HeliosRunFailed) as error:
        run_side(driver, tmp_path, images_of(driver, tmp_path, paired=True))
    assert str(inspector) in str(error.value)
    assert "baseline" in str(error.value)


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


@pytest.mark.parametrize("reuse", [False, True])
def test_the_driver_parses_the_baseline_flags_the_plan_emits(tmp_path, reuse) -> None:
    """The plan's argv and the driver's parser are edited together."""
    options = RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path,
        advisory=True,
        sides=frozenset({Side.HELIOS, Side.HELIOS_BASELINE}),
        baseline=Baseline(ref="merge-base", sha="a" * 40, worktree=tmp_path / "worktree"),
        network=NetworkOptions(reuse_host_listeners=reuse),
    )
    commands = [
        command for command in plan(options, load_manifest(), []) if command.argv[1:2] == [str(GAP_BENCH)]
    ]
    assert len(commands) == 1, "a paired run drives the Helios side once, for both images"
    parsed = gap_bench().build_parser().parse_args(commands[0].argv[2:])
    assert parsed.helios_baseline_root == tmp_path / "worktree"
    assert parsed.helios_baseline_out_dir == tmp_path / "helios-baseline"
    assert parsed.reuse_host_listeners == reuse
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
        driver.HostServices(tmp_path, "10.77.0.1", "10.77.0.1"),
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
    # Beside each kernel's commit, the commit its inspector was built
    # from (#356) — the pairing's tooling provenance survives the round
    # trip too.
    assert read_back.run.inspector_git_sha == paired_regression_report.run.helios_git_sha
    assert read_back.run.baseline_inspector_git_sha == (paired_regression_report.run.baseline_git_sha)
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


def test_a_paired_report_names_each_side_tooling_revision(paired_regression_report: Report) -> None:
    """The rendered report prints the inspector revision beside each kernel's.

    The fields are the same shape as the kernel shas they sit beside —
    the commit the side's `helios-inspector`/`helios-cli` were built from
    (#356) — and the header and the pins table carry them so a paired
    report says which tooling booted which image.
    """
    run = paired_regression_report.run
    assert run.inspector_git_sha == run.helios_git_sha
    assert run.baseline_inspector_git_sha == run.baseline_git_sha

    tables = render_tables(paired_regression_report)
    assert f"inspector `{run.helios_git_sha[:12]}`" in tables
    assert f"inspector `{run.baseline_git_sha[:12]}`" in tables
    pins = render_pins(paired_regression_report)
    assert f"| `helios-inspector`/`helios-cli` | `{run.inspector_git_sha}` |" in pins
    assert f"| `helios-inspector`/`helios-cli`, baseline | `{run.baseline_inspector_git_sha}` |" in pins


def test_an_unpaired_report_keeps_its_three_columns(baseline_report: Report) -> None:
    assert not baseline_report.run.paired
    assert baseline_report.table_sides() == [Side.HELIOS, Side.LINUX_WASMTIME, Side.LINUX_NATIVE]
    assert evaluate_paired(baseline_report) is None


def test_a_build_record_with_an_uncovered_count_names_it_in_the_gate(
    paired_regression_report: Report,
) -> None:
    """#329: the figure a profile-use build counted rides the column's label.

    Two profile-use columns pair identical builds against two profiles —
    the uncovered count is part of what each column was, and the gate
    names it the way it names the profile.
    """
    run = paired_regression_report.run.model_copy(
        update={
            "kernel_build": "profile-use",
            "baseline_kernel_build": "profile-use",
            "kernel_profile": "dev@abc1234 run 34424416974",
            "baseline_kernel_profile": "release helios-v0.1.0",
            "kernel_pgo_uncovered": 34166,
            "kernel_pgo_functions": 42053,
            "baseline_kernel_pgo_uncovered": 7897,
            "baseline_kernel_pgo_functions": 42053,
        }
    )
    report = paired_regression_report.model_copy(update={"run": run})

    verdict = evaluate_paired(report)
    assert "34,166 of 42,053 functions uncovered" in verdict.candidate_label
    assert "7,897 of 42,053 functions uncovered" in verdict.baseline_label
    gate_text = render_gate(gate_report(report, None), report.run.lane)
    assert "34,166 of 42,053 functions uncovered" in gate_text
    assert "7,897 of 42,053 functions uncovered" in gate_text


def test_the_uncovered_count_is_read_from_the_list_beside_the_kernel(tmp_path) -> None:
    """The runner asks the inspector where the kernel is and counts its list.

    The list lives beside whatever `kernel-path` answers rather than under
    a path spelled twice, so the stand-in's keying is what the test counts
    through. The inspector asked is the workspace's own — where a paired
    run leaves each side's tooling (#356).
    """
    lane = load_manifest().lane("x86-64-kvm")
    checkout = fake_checkout(tmp_path / "candidate")
    inspector = fake_inspector(checkout / "target" / "release")
    answered = subprocess.run(
        [
            str(inspector),
            "vm",
            "--arch",
            lane.helios_arch,
            "--release",
            "--accel",
            lane.accelerator,
            "kernel-path",
        ],
        env={**os.environ, "HELIOS_WORKSPACE_ROOT": str(checkout)},
        capture_output=True,
        text=True,
        check=True,
    )
    kernel = Path(answered.stdout.strip())
    listing = Path(str(kernel) + ".pgo-uncovered.txt")
    listing.write_text(
        "# uncovered: 3 of 42 functions (7.1%)\n"
        "# warnings emitted: 3 in 2 crates\n"
        "warning: a.1-cgu.0: no profile data available for function _A Hash = 1 up to 0 count discarded\n"
        "warning: a.1-cgu.0: no profile data available for function _B Hash = 2 up to 0 count discarded\n"
        "warning: b.2-cgu.3: no profile data available for function _C Hash = 3 up to 0 count discarded\n",
        encoding="utf-8",
    )
    assert kernel_pgo_uncovered(checkout, lane, None) == (3, 42)

    # A kernel whose build kept no list reports no count rather than zero.
    plain = fake_checkout(tmp_path / "plain")
    fake_inspector(plain / "target" / "release")
    assert kernel_pgo_uncovered(plain, lane, None) is None

    # And a list whose header disagrees with its lines is a broken
    # artifact, not a count.
    listing.write_text(
        "# uncovered: 4 of 42 functions (9.5%)\n"
        "# warnings emitted: 4 in 2 crates\n"
        "warning: a.1-cgu.0: no profile data available for function _A Hash = 1 up to 0 count discarded\n",
        encoding="utf-8",
    )
    with pytest.raises(SystemExit, match="names 4 emitted warnings but lists 1"):
        kernel_pgo_uncovered(checkout, lane, None)


def test_a_kernel_path_that_fails_is_a_failure_not_an_empty_count(tmp_path) -> None:
    """A nonzero `kernel-path` names what refused, rather than reading None."""
    lane = load_manifest().lane("x86-64-kvm")
    checkout = fake_checkout(tmp_path / "candidate")
    inspector = checkout / "target" / "release" / "helios-inspector"
    inspector.parent.mkdir(parents=True)
    inspector.write_text("#!/bin/sh\necho 'the profile is not in the store' >&2\nexit 3\n", encoding="utf-8")
    inspector.chmod(0o755)
    with pytest.raises(SystemExit, match="exited with status 3: the profile is not in the store"):
        kernel_pgo_uncovered(checkout, lane, None)


def test_a_paired_regression_blocks_on_a_shared_runner(paired_regression_report: Report) -> None:
    """The whole point: the report is advisory and the gate still blocks.

    Both columns came out of one job on one host, so no change of machine
    can explain the shift, whatever the runner was.
    """
    assert not paired_regression_report.run.publishable
    result = evaluate_paired(paired_regression_report)
    rows = {(row.workload, row.measurement): row for row in result.rows}

    assert result.kind is GateKind.PAIRED
    assert result.enforced and result.blocking
    assert rows["hostcall-loop", "elapsed_ms"].regression
    assert rows["hostcall-loop", "elapsed_ms"].shift == pytest.approx(0.5, abs=0.1)
    assert not rows["quickjs-loop", "elapsed_ms"].regression
    # The workload's own measurements moved with it, and the rate metric
    # moved the other way for the same reason.
    assert rows["hostcall-loop", "rtt_p50_us"].regression
    assert rows["hostcall-loop", "switches_per_s"].regression
    assert rows["hostcall-loop", "switches_per_s"].shift < 0


def test_a_paired_improvement_is_named_and_blocks_nothing(paired_improvement_report: Report) -> None:
    result = evaluate_paired(paired_improvement_report)
    rows = {(row.workload, row.measurement): row for row in result.rows}

    assert not result.blocking
    assert rows["hostcall-loop", "elapsed_ms"].improvement
    assert rows["hostcall-loop", "elapsed_ms"].shift == pytest.approx(-0.33, abs=0.05)
    assert [row.measurement for row in result.improvements] == [
        "elapsed_ms",
        "rtt_p50_us",
        "rtt_p99_us",
        "switches_per_s",
    ]
    # More switches per second is the improvement, and the row says so
    # rather than reading the positive shift as a regression.
    assert rows["hostcall-loop", "switches_per_s"].shift > 0


def test_a_paired_shift_inside_the_floor_is_neither(paired_flat_report: Report) -> None:
    """A few tenths of a percent is the machine, not the change."""
    result = evaluate_paired(paired_flat_report)
    rows = {(row.workload, row.measurement): row for row in result.rows}

    assert result.noise_floor > 0
    for measurement in ("elapsed_ms", "rtt_p50_us", "switches_per_s"):
        row = rows["hostcall-loop", measurement]
        assert abs(row.shift) < result.noise_floor
        assert not row.beyond_noise
        assert not row.regression and not row.improvement
    assert not result.blocking


def test_a_floor_past_the_bound_is_inconclusive_and_fails_the_check(
    paired_noisy_host_report: Report,
) -> None:
    """Issue #292: a 28.7% floor let four rows through as regressions and
    the check stayed green. Past the dispersion bound the run measured the
    host, so no row is judged and the enforced comparison blocks."""
    result = evaluate_paired(paired_noisy_host_report)

    assert result.noise_floor > result.floor_bound == 0.15
    assert result.inconclusive and result.blocking
    assert result.regressions == [] and result.improvements == [] and result.headline_regressions == []
    # The rows were still computed and the shift is real on the page: the
    # verdict is what the floor withholds.
    rows = {(row.workload, row.measurement): row for row in result.rows}
    assert rows["hostcall-loop", "elapsed_ms"].shift == pytest.approx(0.5, abs=0.1)
    assert result.control is not None
    assert result.control.side is Side.HELIOS
    assert result.control.drift == pytest.approx(0.287, abs=0.02)

    text = render_gate(gate_report(paired_noisy_host_report, None), "x86-64-kvm")
    assert "**Blocking: inconclusive**" in text
    assert "rerun the lane" in text
    assert "| inconclusive |" in text
    assert "**regression**" not in text
    assert gate_report(paired_noisy_host_report, None).blocking


def test_an_unenforced_inconclusive_comparison_reports_and_blocks_nothing(
    paired_noisy_host_report: Report, baseline_report: Report
) -> None:
    result = evaluate(baseline_report, paired_noisy_host_report)

    assert not result.enforced
    assert result.inconclusive and not result.blocking
    text = render_gate(gate_report(paired_noisy_host_report, baseline_report), "x86-64-kvm")
    assert "Inconclusive — the noise floor" in text
    assert "Blocking" in text  # the paired half of the same report still blocks


def test_a_quiet_host_is_conclusive(paired_flat_report: Report) -> None:
    result = evaluate_paired(paired_flat_report)
    assert not result.inconclusive
    assert "nconclusive" not in render_gate(gate_report(paired_flat_report, None), "x86-64-kvm")


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


def test_a_profile_use_pairing_plans_two_distinct_kernel_paths(tmp_path, monkeypatch) -> None:
    """The two columns of a PGO pairing boot two files.

    Both are `profile-use` builds since a release kernel of this lane
    reads the fetched profile (#226), and one cargo profile is one output
    directory, so until #327 the second build overwrote the first and
    both images resolved to
    `target/x86_64-unknown-none/profile-use/helios`: run 34437890235
    refused itself on the identical-images guard. The driver asks the
    inspector where each image lives, per image and with that image's
    profile in the question, so the pairing turns on the inspector
    keeping the two builds apart — and on nothing this file could paper
    over, which is why the pairing that still names one build twice is
    pinned here beside the one that does not.
    """
    module = gap_bench()
    checkout = fake_checkout(tmp_path / "candidate")
    monkeypatch.setattr(module, "repo_root", lambda: checkout)
    monkeypatch.setenv("HELIOS_INSPECTOR_BIN", str(fake_inspector(tmp_path)))
    profile = tmp_path / "helios-kernel.profdata"
    profile.write_bytes(b"\x00" * 16)
    candidate = module.HeliosImage(
        name="helios",
        workspace_root=checkout,
        out_dir=tmp_path / "out" / "helios",
        profile_use=profile,
    )
    baseline = module.HeliosImage(
        name="helios-baseline",
        workspace_root=checkout,
        out_dir=tmp_path / "out" / "helios-baseline",
    )

    paths = [module.guest_artifact(image, "x86-64", "kvm") for image in (candidate, baseline)]
    assert paths[0] != paths[1], "one checkout, two profiles, two kernel images"
    module.refuse_identical_images([candidate, baseline], "x86-64", "kvm")

    twin = module.HeliosImage(
        name="helios-twin",
        workspace_root=checkout,
        out_dir=tmp_path / "out" / "helios-twin",
        profile_use=profile,
    )
    with pytest.raises(module.IdenticalHeliosImages, match="same guest build"):
        module.refuse_identical_images([candidate, twin], "x86-64", "kvm")


def test_a_plain_baseline_is_the_pgo_control(tmp_path) -> None:
    """`--baseline-kernel-build release` pairs this checkout's
    profile-guided release kernel against the same commit built without
    the fetched profile (#322): the run is paired, the second image is a
    `release` build, and the gap bench is told to build it that way."""
    options = RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path / "out",
        advisory=True,
        sides=frozenset({Side.HELIOS, Side.HELIOS_BASELINE}),
        plain_baseline=True,
    )
    assert options.paired
    assert options.kernel_build == "profile-use"
    assert options.baseline_kernel_build == "release"
    invocations = [
        command.argv
        for command in plan(options, load_manifest(), WORKLOADS)
        if command.argv[1:2] == [str(GAP_BENCH)]
    ]
    assert len(invocations) == 1
    args = gap_bench().build_parser().parse_args(invocations[0][2:])
    assert args.helios_baseline_without_kernel_profile
    assert args.helios_baseline_root is None and args.helios_profile_use is None
    assert args.helios_baseline_out_dir == options.out_dir / "helios-baseline"


def test_the_pgo_control_is_refused_where_the_release_kernel_is_plain(tmp_path) -> None:
    """A lane whose release build reads no profile has no separate
    control: its plain build is the only build, and pairing it against
    itself would be the identical-images refusal one boot later."""
    lane = load_manifest().lane("x86-64-kvm").model_copy(update={"helios_arch": "riscv64"})
    with pytest.raises(SystemExit, match="reads no profile"):
        RunOptions(
            lane=lane,
            out_dir=tmp_path / "out",
            advisory=True,
            sides=frozenset({Side.HELIOS, Side.HELIOS_BASELINE}),
            plain_baseline=True,
        )


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


def test_an_x86_64_release_kernel_is_a_profile_use_build(tmp_path) -> None:
    """There is no plain release kernel on the measured architecture.

    Every x86-64 release build reads the profile the latest release
    published (#226), so the run record names the build directory the
    inspector actually wrote to — which is where the bootfs pins are
    read from as well.
    """
    options = RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path / "out",
        advisory=True,
        sides=frozenset({Side.HELIOS}),
    )
    assert options.kernel_build == "profile-use"
    assert not options.paired
    assert options.baseline_kernel_build is None, "an unpaired run has no second image"


def test_a_pgo_pairing_names_the_profile_each_column_read(paired_regression_report) -> None:
    """Once both columns are profile-use builds, the profile is what varies.

    The baseline reads the profile the release published and the
    candidate the one this run collected, on one commit and one host, so
    the labels name the profiles or the table says nothing about which
    column is which.
    """
    sha = paired_regression_report.run.helios_git_sha
    run = paired_regression_report.run.model_copy(
        update={
            "baseline_git_sha": sha,
            "baseline_ref": None,
            "kernel_build": "profile-use",
            "baseline_kernel_build": "profile-use",
            "kernel_profile": "target/pgo-candidate/helios-kernel.profdata",
            "baseline_kernel_profile": "release helios-v0.1.0",
        }
    )
    result = evaluate_paired(paired_regression_report.model_copy(update={"run": run}))

    assert result is not None and result.kind is GateKind.PAIRED
    assert "release helios-v0.1.0" in result.baseline_label
    assert "target/pgo-candidate/helios-kernel.profdata" in result.candidate_label
    assert result.baseline_label != result.candidate_label
