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
    assert {lane.name for lane in manifest.lanes} == {"x86-64-kvm", "aarch64-hvf"}
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
