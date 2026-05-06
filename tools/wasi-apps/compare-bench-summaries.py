#!/usr/bin/env python3
import argparse
import json
from pathlib import Path


def load_summary(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as handle:
        summary = json.load(handle)
    if summary.get("schema_version") != 1:
        raise SystemExit(f"unsupported summary schema_version in {path}")
    return summary


def workload_map(summary: dict) -> dict[str, dict]:
    return {workload["name"]: workload for workload in summary.get("workloads", [])}


def hotspot_map(summary: dict) -> dict[str, dict]:
    return {hotspot["name"]: hotspot for hotspot in summary.get("network_hotspots", [])}


def ratio(candidate: float | int | None, baseline: float | int | None) -> float | None:
    if candidate is None or baseline in (None, 0):
        return None
    return candidate / baseline


def format_float(value: float | None, suffix: str = "") -> str:
    if value is None:
        return "n/a"
    return f"{value:.3f}{suffix}"


def format_ms(value: float | None) -> str:
    return format_float(value, " ms")


def format_ratio(value: float | None) -> str:
    return format_float(value, "x")


def write_markdown(path: Path, baseline: dict, candidate: dict) -> None:
    baseline_workloads = workload_map(baseline)
    candidate_workloads = workload_map(candidate)
    names = sorted(set(baseline_workloads) | set(candidate_workloads))

    lines = [
        "# Benchmark Summary Comparison",
        "",
        f"- Baseline SHA: `{baseline.get('helios_git_sha', 'unknown')}`",
        f"- Candidate SHA: `{candidate.get('helios_git_sha', 'unknown')}`",
        "",
        "## Workloads",
        "",
        "| Workload | Baseline Helios | Candidate Helios | Candidate/Baseline | Baseline H/L | Candidate H/L |",
        "| --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    for name in names:
        base = baseline_workloads.get(name, {})
        cand = candidate_workloads.get(name, {})
        base_ms = base.get("helios_median_ms")
        cand_ms = cand.get("helios_median_ms")
        lines.append(
            f"| `{name}` | {format_ms(base_ms)} | {format_ms(cand_ms)} | "
            f"{format_ratio(ratio(cand_ms, base_ms))} | "
            f"{format_ratio(base.get('helios_to_linux_ratio'))} | "
            f"{format_ratio(cand.get('helios_to_linux_ratio'))} |"
        )

    baseline_hotspots = hotspot_map(baseline)
    candidate_hotspots = hotspot_map(candidate)
    hotspot_names = sorted(
        set(baseline_hotspots) | set(candidate_hotspots),
        key=lambda name: candidate_hotspots.get(name, baseline_hotspots.get(name, {})).get(
            "total_nanos",
            0,
        ),
        reverse=True,
    )
    lines.extend(
        [
            "",
            "## Network Hotspots",
            "",
            "| Metric | Baseline total | Candidate total | Candidate/Baseline | Baseline ns/event | Candidate ns/event |",
            "| --- | ---: | ---: | ---: | ---: | ---: |",
        ]
    )
    for name in hotspot_names[:20]:
        base = baseline_hotspots.get(name, {})
        cand = candidate_hotspots.get(name, {})
        base_nanos = base.get("total_nanos")
        cand_nanos = cand.get("total_nanos")
        lines.append(
            f"| `{name}` | {format_float(base_nanos, ' ns')} | "
            f"{format_float(cand_nanos, ' ns')} | "
            f"{format_ratio(ratio(cand_nanos, base_nanos))} | "
            f"{format_float(base.get('nanos_per_event'))} | "
            f"{format_float(cand.get('nanos_per_event'))} |"
        )
    lines.append("")
    path.write_text("\n".join(lines), encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    write_markdown(args.out, load_summary(args.baseline), load_summary(args.candidate))
    print(args.out)


if __name__ == "__main__":
    main()
