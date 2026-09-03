"""Synthetic reports for the statistics, gate and rendering tests.

Nothing here comes from a benchmark run; the numbers are generated from
fixed seeds so the tests are deterministic and the tooling can be
exercised on a machine that never runs the suite.
"""

from __future__ import annotations

import numpy as np
import pytest

from helios_bench.assemble import assemble_report, build_control
from helios_bench.report import Hardware, Iteration, Pins, Report, RunInfo, Side, Thresholds
from helios_bench.sources import RawCell, RawSide

THRESHOLDS = Thresholds(
    iterations=11,
    warmup_discard=1,
    cv_bound=0.15,
    bootstrap_resamples=2000,
    confidence=0.95,
    bootstrap_seed=7,
)

WORKLOADS = [
    {
        "name": "hostcall-loop",
        "class": "hostcall",
        "headline": True,
        "description": "host call loop",
        "counterparts": {"linux_native": {}, "linux_wasmtime": {}},
    },
    {
        "name": "quickjs-loop",
        "class": "compute",
        "headline": True,
        "description": "compute parity",
        "counterparts": {"linux_native": {}, "linux_wasmtime": {}},
    },
    {
        "name": "fs-smallfiles",
        "class": "fs",
        "headline": False,
        "description": "small files",
        "throughput_bytes": None,
        "counterparts": {"linux_native": {}, "linux_wasmtime": None},
    },
]


def iterations(
    center: float, spread: float, seed: int, count: int = 11, cold_extra: float = 0.0
) -> list[Iteration]:
    generator = np.random.default_rng(seed)
    values = generator.normal(center, spread, size=count)
    values[0] += cold_extra
    return [
        Iteration(index=index + 1, elapsed_ms=float(max(value, 0.01)), cold=index == 0, metrics={"x": 1.0})
        for index, value in enumerate(values)
    ]


def raw_side(cells: dict[str, list[Iteration]]) -> RawSide:
    return RawSide(
        run={"type": "run"}, cells={name: RawCell(iterations=items) for name, items in cells.items()}
    )


def make_report(
    helios_centers: dict[str, float],
    run_id: str = "1001",
    publishable: bool = True,
    seed: int = 1,
) -> Report:
    sides = {
        Side.HELIOS: raw_side(
            {
                name: iterations(center, center * 0.02, seed + sum(map(ord, name)) % 100, cold_extra=center)
                for name, center in helios_centers.items()
            }
        ),
        Side.LINUX_WASMTIME: raw_side(
            {
                "hostcall-loop": iterations(200.0, 4.0, seed + 11),
                "quickjs-loop": iterations(100.0, 2.0, seed + 12),
            }
        ),
        Side.LINUX_NATIVE: raw_side(
            {
                "hostcall-loop": iterations(50.0, 1.0, seed + 21),
                "quickjs-loop": iterations(80.0, 1.6, seed + 22),
                "fs-smallfiles": iterations(30.0, 0.6, seed + 23),
            }
        ),
    }
    control = build_control(
        "quickjs-loop",
        {
            Side.HELIOS: (
                raw_side({"quickjs-loop": iterations(100.0, 1.0, seed + 31)}),
                raw_side({"quickjs-loop": iterations(101.0, 1.0, seed + 32)}),
            )
        },
        THRESHOLDS,
    )
    run = RunInfo(
        id=run_id,
        url=f"https://github.com/lexoliu/helios/actions/runs/{run_id}",
        attempt=1,
        lane="aarch64-hvf",
        runner_label="helios-bench-arm-hvf" if publishable else "macos-15",
        advisory=not publishable,
        publishable=publishable,
        deviations=[] if publishable else ["qemu-system-aarch64 is 10.0.0, lane pins 10.2.2"],
        started_at="2026-09-03T00:00:00+00:00",
        finished_at="2026-09-03T01:00:00+00:00",
        helios_git_sha="0123456789abcdef0123456789abcdef01234567",
    )
    hardware = Hardware(
        host_os="Darwin 25.6.0",
        host_arch="aarch64",
        cpu="Apple M4 Pro",
        logical_cpus=12,
        memory_bytes=48 << 30,
        accelerator="hvf",
        qemu_version="10.2.2",
    )
    pins = Pins(
        wasmtime_revision="b83d18c8558b6d32fb0c0727d1c6a32639842c49",
        wasmtime_linux_release="wasmtime-v48.0.0-aarch64-linux",
        fedora_image_url="https://download.fedoraproject.org/example.qcow2",
        fedora_image_sha256="55c60a3b80d3616a08705afd0459e75fe9f03c54aba7a46e4002a41a72fa0d5b",
        qemu_version="10.2.2",
        vcpus=4,
        memory="2G",
        linux_vm_memory="4G",
        net_backend="user",
        devices=["virtio-blk-device", "virtio-net-device", "virtio-rng-device"],
        wasm_artifacts={"artifacts/wasi-tools/hostcall-loop.wasm": "ab" * 32},
        bootfs_cwasm={"hostcall-loop_bootfs_component.cwasm": "cd" * 32},
    )
    return assemble_report(WORKLOADS, sides, control, run, hardware, pins, THRESHOLDS)


@pytest.fixture
def baseline_report() -> Report:
    return make_report({"hostcall-loop": 20.0, "quickjs-loop": 100.5, "fs-smallfiles": 25.0})


@pytest.fixture
def regressed_report() -> Report:
    return make_report(
        {"hostcall-loop": 30.0, "quickjs-loop": 100.5, "fs-smallfiles": 25.0}, run_id="1002", seed=5
    )


@pytest.fixture
def advisory_report() -> Report:
    return make_report(
        {"hostcall-loop": 20.0, "quickjs-loop": 130.0, "fs-smallfiles": 25.0},
        run_id="1003",
        publishable=False,
        seed=9,
    )
