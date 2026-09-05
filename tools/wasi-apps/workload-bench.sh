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
)

if [[ -n "${HELIOS_WORKLOAD_BENCH_VM_MEMORY:-}" ]]; then
    command+=(--memory "${HELIOS_WORKLOAD_BENCH_VM_MEMORY}")
fi

# The inspector requires the profile's native accelerator when nobody
# names one, so a host that means to measure the emulator has to say so.
# Naming it here also puts the accelerator in the run record every
# summary is read back from.
if [[ -n "${HELIOS_WORKLOAD_BENCH_ACCEL:-}" ]]; then
    command+=(--accel "${HELIOS_WORKLOAD_BENCH_ACCEL}")
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_VM_SMP:-}" ]]; then
    command+=(--smp "${HELIOS_WORKLOAD_BENCH_VM_SMP}")
fi

# A benchmark that never boots leaves nothing behind to say why. Keeping
# the runtime directory puts the guest console, QEMU's own stderr and the
# inspector's log on disk, which is the only place a CI lane can collect
# a bring-up failure from.
#
# One subdirectory per invocation, named after this invocation's log: a
# benchmark run boots one guest per workload class through this script,
# and they cannot share a directory. The boot image and the debug socket
# live at fixed names inside it, so the next guest's QEMU fails to take
# the write lock on `kernel.uefi.img` while the previous one still holds
# it, and two guests would otherwise bind one `debug.sock`.
if [[ -n "${HELIOS_WORKLOAD_BENCH_RUNTIME_DIR:-}" ]]; then
    runtime_dir="${HELIOS_WORKLOAD_BENCH_RUNTIME_DIR}/$(basename "${log%.jsonl}")"
    mkdir -p "${runtime_dir}"
    command+=(--runtime-dir "${runtime_dir}" --keep-runtime-dir)
    # A network workload that fails leaves an exit status and nothing
    # about the wire. The capture sits between the virtio-net device and
    # the host backend, so it records what the guest driver actually
    # sent and received; it lives in the runtime directory because that
    # is already one per boot, and a shared path would let the second
    # workload class overwrite the first's capture.
    if [[ -n "${HELIOS_WORKLOAD_BENCH_NET_PCAP:-}" ]]; then
        command+=(--net-pcap "${runtime_dir}/net.pcap")
    fi
fi

# Host packet path for the guest's virtio-net device. Only a multi-queue
# backend can exercise the driver's multiqueue and offload paths; see
# docs/networking.md.
if [[ -n "${HELIOS_WORKLOAD_BENCH_NET_BACKEND:-}" ]]; then
    command+=(--net-backend "${HELIOS_WORKLOAD_BENCH_NET_BACKEND}")
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_NET_QUEUES:-}" ]]; then
    command+=(--net-queues "${HELIOS_WORKLOAD_BENCH_NET_QUEUES}")
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_NET_IFNAME:-}" ]]; then
    command+=(--net-ifname "${HELIOS_WORKLOAD_BENCH_NET_IFNAME}")
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_NET_BRIDGE:-}" ]]; then
    command+=(--net-bridge "${HELIOS_WORKLOAD_BENCH_NET_BRIDGE}")
fi

command+=(
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

if [[ -n "${HELIOS_WORKLOAD_BENCH_HOST_TCP_HOST:-}" ]]; then
    command+=(--host-tcp-host "${HELIOS_WORKLOAD_BENCH_HOST_TCP_HOST}")
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_HOST_TCP_PORT:-}" ]]; then
    command+=(--host-tcp-port "${HELIOS_WORKLOAD_BENCH_HOST_TCP_PORT}")
fi

# A workload runs inside the guest and is reported by an RPC that never
# answers if the guest stops making progress, so the runner fails an
# iteration that overruns rather than holding the lane. The inspector's
# own default applies unless a slower surface asks for more.
if [[ -n "${HELIOS_WORKLOAD_BENCH_WORKLOAD_TIMEOUT_SECONDS:-}" ]]; then
    command+=(--workload-timeout-seconds "${HELIOS_WORKLOAD_BENCH_WORKLOAD_TIMEOUT_SECONDS}")
fi

if [[ -n "${HELIOS_WORKLOAD_BENCH_HOST_TCP_ECHO_PORT:-}" ]]; then
    command+=(--host-tcp-echo-port "${HELIOS_WORKLOAD_BENCH_HOST_TCP_ECHO_PORT}")
fi
if [[ "${HELIOS_WORKLOAD_BENCH_KEEP_GOING:-0}" == "1" ]]; then
    command+=(--keep-going)
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
