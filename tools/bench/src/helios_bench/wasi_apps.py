"""Access to the modules under tools/wasi-apps that own the pins.

They are plain scripts, not a package, so they are imported by path. Every
value the suite takes from them (the Fedora image digest, the Wasmtime
Linux release, the workload manifest interpretation) has exactly one
definition, in those scripts.
"""

from __future__ import annotations

import importlib
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
