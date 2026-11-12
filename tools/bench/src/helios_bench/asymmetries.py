"""What still differs between a lane's two columns by construction (#491).

The report's pins name each difference next to the values the run read:
the Wasmtime each side runs, stack-limit interruption, and the network
backend. A value a run can read is read from where it is pinned; a fact
fixed by how the lane builds and runs each side is the lane's own data
in `manifest.toml`.
"""

from __future__ import annotations

from helios_bench.manifest import Lane
from helios_bench.report import Asymmetry


def collect_asymmetries(lane: Lane, wasmtime_revision: str, wasmtime_linux_release: str) -> list[Asymmetry]:
    """The differences the lane's pins state, empty for a lane that states none."""
    stated = lane.stated_asymmetries
    if stated is None:
        return []
    return [
        Asymmetry(
            what="Wasmtime",
            helios=f"fork revision `{wasmtime_revision}`",
            linux=f"release `{wasmtime_linux_release}`",
        ),
        Asymmetry(
            what="Stack-limit interruption",
            helios=stated.stack_limit_interruption.helios,
            linux=stated.stack_limit_interruption.linux,
        ),
        Asymmetry(
            what="Network class backend",
            helios=f"QEMU `{lane.net_backend}`",
            linux=f"QEMU `{lane.net_backend}`",
        ),
    ]
