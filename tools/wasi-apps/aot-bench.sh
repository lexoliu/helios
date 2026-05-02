#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"

cargo_bin="${CARGO:-cargo}"
input="${HELIOS_AOT_BENCH_INPUT:-artifacts/wasi-tools/curl.wasm}"
iterations="${HELIOS_AOT_BENCH_ITERATIONS:-5}"
arch="${HELIOS_AOT_BENCH_ARCH:-aarch64}"
target_label="${HELIOS_AOT_BENCH_TARGET_LABEL:-${arch}-hvf}"
inspector="${HELIOS_INSPECTOR_BIN:-target/release/helios-inspector}"

if [[ ! "${iterations}" =~ ^[0-9]+$ ]] || (( iterations < 2 )); then
    printf 'HELIOS_AOT_BENCH_ITERATIONS must be an integer >= 2\n' >&2
    exit 1
fi

if [[ ! -f "${input}" ]]; then
    printf 'AOT bench input does not exist: %s\n' "${input}" >&2
    printf 'Run tools/wasi-apps/build.sh to stage WASI artifacts first.\n' >&2
    exit 1
fi

"${cargo_bin}" build --release -p helios-inspector

mkdir -p target/perf-baselines
short_sha="$(git rev-parse --short HEAD)"
log="${HELIOS_AOT_BENCH_LOG:-target/perf-baselines/${target_label}-curl-${short_sha}.log}"

command=(
    "${inspector}"
    vm
    --arch
    "${arch}"
    --release
    aot-bench
    "${input}"
    --iterations
    "${iterations}"
)

if [[ "${HELIOS_AOT_BENCH_COMPILER_TIMING:-0}" == "1" ]]; then
    command+=(--compiler-timing)
fi

printf 'writing AOT benchmark log to %s\n' "${log}" >&2
"${command[@]}" | tee "${log}"

median="$(
    sed -nE 's/^iteration=([0-9]+) elapsed_ms=([0-9]+).*/\1 \2/p' "${log}" \
        | awk '$1 > 1 { print $2 }' \
        | sort -n \
        | awk '
            { values[NR] = $1 }
            END {
                if (NR == 0) {
                    exit 1
                }
                lower = int((NR + 1) / 2)
                upper = int((NR + 2) / 2)
                print int((values[lower] + values[upper]) / 2)
            }
        '
)"

printf 'steady_state_median_elapsed_ms=%s log=%s\n' "${median}" "${log}"
