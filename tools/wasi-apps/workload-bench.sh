#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"

cargo_bin="${CARGO:-cargo}"
arch="${HELIOS_WORKLOAD_BENCH_ARCH:-aarch64}"
target_label="${HELIOS_WORKLOAD_BENCH_TARGET_LABEL:-${arch}-hvf}"
iterations="${HELIOS_WORKLOAD_BENCH_ITERATIONS:-3}"
inspector="${HELIOS_INSPECTOR_BIN:-target/release/helios-inspector}"

if [[ ! "${iterations}" =~ ^[0-9]+$ ]] || (( iterations == 0 )); then
    printf 'HELIOS_WORKLOAD_BENCH_ITERATIONS must be a non-zero integer\n' >&2
    exit 1
fi

"${cargo_bin}" build --release -p helios-inspector

mkdir -p target/perf-baselines
short_sha="$(git rev-parse --short HEAD)"
log="${HELIOS_WORKLOAD_BENCH_LOG:-target/perf-baselines/${target_label}-wasi-workloads-${short_sha}.log}"

command=(
    "${inspector}"
    vm
    --arch
    "${arch}"
    --release
    workload-bench
    --iterations
    "${iterations}"
)

if [[ -n "${HELIOS_WORKLOAD_BENCH_WORKLOADS:-}" ]]; then
    IFS=',' read -r -a workloads <<<"${HELIOS_WORKLOAD_BENCH_WORKLOADS}"
    for workload in "${workloads[@]}"; do
        command+=(--workload "${workload}")
    done
fi

printf 'writing WASI workload benchmark log to %s\n' "${log}" >&2
"${command[@]}" | tee "${log}"
