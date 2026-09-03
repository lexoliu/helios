"""Helios benchmark suite tooling.

The package turns the three-way comparison (Helios, Linux + Wasmtime,
native Linux) into a reproducible artifact: a typed manifest of pins, a
runner that refuses hosts that deviate from it, statistics with bootstrap
confidence intervals, one JSON report per run, rendered tables and plots,
and a regression gate between two reports.
"""

from pathlib import Path

PACKAGE_ROOT = Path(__file__).resolve().parent
TOOLS_BENCH_ROOT = PACKAGE_ROOT.parents[1]
REPO_ROOT = TOOLS_BENCH_ROOT.parents[1]
WASI_APPS_ROOT = REPO_ROOT / "tools" / "wasi-apps"
