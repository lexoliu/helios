"""The workload manifest as the suite reads it."""

from __future__ import annotations

from helios_bench.manifest import WORKLOADS_PATH
from helios_bench.wasi_apps import workload_runner


def load_workloads() -> dict:
    return workload_runner().load_manifest(WORKLOADS_PATH)


def select_workloads(manifest: dict, names: list[str]) -> list[dict]:
    if not names:
        return list(manifest["workloads"])
    by_name = {workload["name"]: workload for workload in manifest["workloads"]}
    missing = [name for name in names if name not in by_name]
    if missing:
        raise SystemExit(f"unknown workloads: {', '.join(missing)}")
    return [by_name[name] for name in names]
