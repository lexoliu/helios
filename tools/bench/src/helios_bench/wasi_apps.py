"""Access to the modules under tools/wasi-apps that own the pins.

They are plain scripts, not a package, so they are imported by path. Every
value the suite takes from them (the Fedora image digest, the Wasmtime
Linux release, the workload manifest interpretation) has exactly one
definition, in those scripts.
"""

from __future__ import annotations

import importlib
import importlib.util
import sys
from types import ModuleType

from helios_bench import WASI_APPS_ROOT


def _module(name: str) -> ModuleType:
    if str(WASI_APPS_ROOT) not in sys.path:
        sys.path.insert(0, str(WASI_APPS_ROOT))
    return importlib.import_module(name)


def fedora_baseline() -> ModuleType:
    return _module("fedora_qemu_baseline")


def workload_runner() -> ModuleType:
    return _module("linux_workload_runner")


def gap_bench() -> ModuleType:
    """`linux-gap-bench.py`, the driver that runs the three sides.

    Its file name is not an identifier, so it is loaded by path rather
    than imported; the suite needs it for the run-isolation rules that
    decide which cells a lost guest costs.
    """
    if str(WASI_APPS_ROOT) not in sys.path:
        sys.path.insert(0, str(WASI_APPS_ROOT))
    name = "linux_gap_bench"
    if name in sys.modules:
        return sys.modules[name]
    path = WASI_APPS_ROOT / "linux-gap-bench.py"
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise SystemExit(f"{path}: cannot be loaded as a module")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module
