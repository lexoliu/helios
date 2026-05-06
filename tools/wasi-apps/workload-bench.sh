#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"

cargo_bin="${CARGO:-cargo}"
arch="${HELIOS_WORKLOAD_BENCH_ARCH:-aarch64}"
target_label="${HELIOS_WORKLOAD_BENCH_TARGET_LABEL:-${arch}-hvf}"
iterations="${HELIOS_WORKLOAD_BENCH_ITERATIONS:-5}"
inspector="${HELIOS_INSPECTOR_BIN:-target/release/helios-inspector}"
manifest="${HELIOS_WORKLOAD_BENCH_MANIFEST:-tools/wasi-apps/workloads.json}"

if [[ ! "${iterations}" =~ ^[0-9]+$ ]] || (( iterations == 0 )); then
    printf 'HELIOS_WORKLOAD_BENCH_ITERATIONS must be a non-zero integer\n' >&2
    exit 1
fi

"${cargo_bin}" build --release -p helios-inspector

mkdir -p target/perf-baselines
short_sha="$(git rev-parse --short HEAD)"
log="${HELIOS_WORKLOAD_BENCH_LOG:-target/perf-baselines/${target_label}-wasi-workloads-${short_sha}.jsonl}"

command=(
    "${inspector}"
    vm
    --arch
    "${arch}"
    --release
    workload-bench
    --manifest
    "${manifest}"
    --iterations
    "${iterations}"
)

if [[ -n "${HELIOS_WORKLOAD_BENCH_CLASSES:-}" ]]; then
    IFS=',' read -r -a classes <<<"${HELIOS_WORKLOAD_BENCH_CLASSES}"
    for class in "${classes[@]}"; do
        command+=(--class "${class}")
    done
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_WORKLOADS:-}" ]]; then
    IFS=',' read -r -a workloads <<<"${HELIOS_WORKLOAD_BENCH_WORKLOADS}"
    for workload in "${workloads[@]}"; do
        command+=(--workload "${workload}")
    done
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_HOST_HTTP_URL:-}" ]]; then
    command+=(--host-http-url "${HELIOS_WORKLOAD_BENCH_HOST_HTTP_URL}")
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_PROFILE_OUTPUT:-}" ]]; then
    command+=(--profile-output "${HELIOS_WORKLOAD_BENCH_PROFILE_OUTPUT}")
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_KERNEL_PROFILE_OUTPUT:-}" ]]; then
    command+=(--kernel-profile-output "${HELIOS_WORKLOAD_BENCH_KERNEL_PROFILE_OUTPUT}")
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_USER_PROFILE_OUTPUT:-}" ]]; then
    command+=(--user-profile-output "${HELIOS_WORKLOAD_BENCH_USER_PROFILE_OUTPUT}")
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_PERF_METRICS_OUTPUT:-}" ]]; then
    command+=(--perf-metrics-output "${HELIOS_WORKLOAD_BENCH_PERF_METRICS_OUTPUT}")
fi

printf 'writing WASI workload benchmark JSONL to %s\n' "${log}" >&2
"${command[@]}" | tee "${log}"
