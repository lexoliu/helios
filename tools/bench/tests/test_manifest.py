from pathlib import Path

from helios_bench import REPO_ROOT
from helios_bench.manifest import (
    Lane,
    fedora_image,
    load_manifest,
    vendored_wasmtime_revision,
    wasmtime_linux_release,
)
from helios_bench.report import WorkloadClass
from helios_bench.workloads import load_workloads


def test_manifest_lanes_are_complete() -> None:
    manifest = load_manifest()
    assert {lane.name for lane in manifest.lanes} == {"x86-64-kvm"}
    for lane in manifest.lanes:
        assert isinstance(lane, Lane)
        assert lane.runner_label.startswith("helios-bench-")
        assert lane.qemu_binary == f"qemu-system-{lane.guest_arch}"
    statistics = manifest.statistics
    assert statistics.iterations - statistics.warmup_discard >= 10
    assert statistics.bootstrap_resamples == 10000


def test_pins_come_from_their_single_sources() -> None:
    revision = vendored_wasmtime_revision()
    assert len(revision) == 40
    action = (REPO_ROOT / ".github/actions/checkout-wasmtime/action.yml").read_text(encoding="utf-8")
    assert revision in action
    url, digest = fedora_image("aarch64")
    assert url.endswith(".qcow2") and len(digest) == 64
    assert wasmtime_linux_release("x86_64").startswith("wasmtime-v")


def test_workload_manifest_has_a_headline_per_class_and_a_control() -> None:
    manifest = load_workloads()
    workloads = manifest["workloads"]
    names = {workload["name"] for workload in workloads}
    assert manifest["control_workload"] in names
    for workload_class in WorkloadClass:
        assert any(workload["class"] == workload_class and workload["headline"] for workload in workloads), (
            workload_class
        )
    for workload in workloads:
        assert set(workload["counterparts"]) == {"linux_native", "linux_wasmtime"}


def test_every_row_names_a_wasmtime_counterpart_or_the_reason_it_lacks_one() -> None:
    """#311: no workload may be slower than Linux + Wasmtime, so a row
    without that cell must say why the comparison does not exist."""
    manifest = load_workloads()
    missing = []
    for workload in manifest["workloads"]:
        if workload["counterparts"]["linux_wasmtime"] is None:
            missing.append(workload["name"])
            assert "linux_wasmtime" in workload.get("uncompared", {}), workload["name"]
    assert missing == ["sched-tasks"]

    by_name = {workload["name"]: workload for workload in manifest["workloads"]}
    # The 500-instance row keeps its counterpart commands but the lane
    # skips it on every side; the recorded reason is what the table shows.
    five_hundred = by_name["instance-startup-500"]
    assert set(five_hundred["uncompared"]) == {"linux_native", "linux_wasmtime"}
    for workload in manifest["workloads"]:
        for side, reason in workload.get("uncompared", {}).items():
            assert side in ("linux_native", "linux_wasmtime", "helios"), (workload["name"], side)
            assert reason.strip(), workload["name"]


def test_native_counterparts_exist_for_every_native_bin_reference() -> None:
    manifest = load_workloads()
    sources = {path.stem for path in (REPO_ROOT / "tools/bench/native").glob("*.c")}
    for workload in manifest["workloads"]:
        for side in ("linux_native", "linux_wasmtime"):
            spec = workload["counterparts"][side]
            if not spec or "program" not in spec:
                continue
            for value in [spec["program"], *spec.get("args", [])]:
                if value.startswith("{native_bin}/"):
                    assert Path(value).name in sources, value


def test_the_suite_measures_one_lane() -> None:
    """One architecture, on purpose.

    Nearly everything this suite measures lives in the cross-platform
    kernel, and no hosted GitHub Arm runner exposes `/dev/kvm`, so a
    second lane would repeat the first through an interpreter. An Arm
    number comes from a dedicated machine, by hand.
    """
    manifest = load_manifest()
    assert [lane.name for lane in manifest.lanes] == ["x86-64-kvm"]
    lane = manifest.lane("x86-64-kvm")
    assert lane.accelerator == "kvm"
    assert lane.runs_on(advisory=True) == "ubuntu-24.04"
    assert lane.runs_on(advisory=False) == "helios-bench-x86-kvm"
