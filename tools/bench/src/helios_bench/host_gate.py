"""The host gate of a run: memory pressure around every side (#491).

A macOS host that pages while a guest is timed moves that guest's
numbers by an amount no control workload bounds, so the run reads
`kern.memorystatus_vm_pressure_level` before and after every side of
every round and is inconclusive unless it read normal each time. Linux
has no reading this tool takes in its place, and the record says so
rather than inventing one.
"""

from __future__ import annotations

import platform
import subprocess
from collections.abc import Callable
from dataclasses import dataclass, field

from helios_bench.report import PRESSURE_NORMAL, HostGate, PressureReading

PRESSURE_SYSCTL = "kern.memorystatus_vm_pressure_level"
MACOS_NOTE = (
    f"`{PRESSURE_SYSCTL}` read before and after every side of every round; "
    f"the run is inconclusive unless it is {PRESSURE_NORMAL} (normal) throughout"
)
LINUX_NOTE = (
    f"not applicable: `{PRESSURE_SYSCTL}` is a macOS interface and no equivalent reading "
    "is taken on a Linux host"
)


def read_pressure_level() -> int:
    """The host's current memory-pressure level, as macOS reports it."""
    output = subprocess.run(["sysctl", "-n", PRESSURE_SYSCTL], capture_output=True, text=True, check=True)
    text = output.stdout.strip()
    if not text.isdigit():
        raise SystemExit(f"`sysctl -n {PRESSURE_SYSCTL}` printed {text!r}, expected a level number")
    return int(text)


@dataclass
class HostGateRecorder:
    """Takes the readings of one run; ``system`` is `platform.system()`.

    Any host that is neither Darwin nor Linux is refused: the gate is
    stated for those two and nothing else is known about the others.
    """

    system: str = field(default_factory=platform.system)
    read: Callable[[], int] = field(default_factory=lambda: read_pressure_level)
    readings: list[PressureReading] = field(default_factory=list)

    def __post_init__(self) -> None:
        if self.system not in ("Darwin", "Linux"):
            raise SystemExit(f"the host gate is defined for Darwin and Linux hosts, not {self.system!r}")

    @property
    def applies(self) -> bool:
        return self.system == "Darwin"

    def take(self, round_number: int, side: str, moment: str) -> None:
        if self.applies:
            self.readings.append(
                PressureReading(round=round_number, side=side, moment=moment, level=self.read())
            )

    def record(self) -> HostGate:
        return HostGate(
            applies=self.applies,
            note=MACOS_NOTE if self.applies else LINUX_NOTE,
            readings=self.readings,
        )


def gate_deviations(gate: HostGate) -> list[str]:
    """One deviation per reading that was not normal; the run is then not publishable."""
    return [
        f"host gate: memory pressure level {reading.level} {reading.moment} the {reading.side} side of "
        f"round {reading.round}, not {PRESSURE_NORMAL} (normal): the run is inconclusive"
        for reading in gate.violations
    ]
