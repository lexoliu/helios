#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"

cargo_bin="${CARGO:-cargo}"
arch="${HELIOS_WASI_SMOKE_ARCH:-aarch64}"
cpu="${HELIOS_WASI_SMOKE_CPU:-max}"
smp="${HELIOS_WASI_SMOKE_SMP:-1}"
# A guest boots, compiles and runs inside this window, so the seeded
# clock may legitimately trail the host by the length of the boot.
readonly WALL_CLOCK_TOLERANCE_SECONDS=60
tmp_files=()

cleanup() {
    rm -f "${tmp_files[@]}"
}

trap cleanup EXIT

build_wasix_conformance_artifact() {
    local name="$1"
    local wat="tools/wasi-apps/wasix-tests/${name}.wat"
    local artifact_dir="artifacts/wasix/${name}"
    local wasm="${artifact_dir}/${name}.wasm"

    if [[ ! -f "${wat}" ]]; then
        printf 'WASIX conformance source is missing: %s\n' "${wat}" >&2
        return 1
    fi
    mkdir -p "${artifact_dir}"
    wasm-tools parse "${wat}" -o "${wasm}"
    wasm-tools validate "${wasm}"
    shasum -a 256 "${wasm}" >"${wasm}.sha256"
    {
        printf 'package=%s\n' 'helios-wasix-conformance'
        printf 'version=%s\n' '0.1.0'
        printf 'source=%s\n' 'tools/wasi-apps/wasix-tests'
        printf 'sha256=%s\n' "$(cut -d' ' -f1 "${wasm}.sha256")"
    } > "${artifact_dir}/SOURCE.txt"
}

check_wasm_imports() {
    local name="$1"
    local artifact="$2"
    shift 2

    if ! command -v wasm-tools >/dev/null 2>&1; then
        printf 'wasm-tools is required to validate %s WASIX imports\n' "${name}" >&2
        return 1
    fi
    if [[ ! -f "${artifact}" ]]; then
        printf 'WASIX artifact for %s is missing: %s\n' "${name}" "${artifact}" >&2
        return 1
    fi

    local dump
    dump="$(mktemp "${TMPDIR:-/tmp}/helios-${name}-imports.XXXXXX.wat")"
    tmp_files+=("${dump}")
    wasm-tools print "${artifact}" >"${dump}"

    local expected
    for expected in "$@"; do
        if ! grep -Fq -- "${expected}" "${dump}"; then
            printf 'WASIX artifact %s does not contain expected import/export: %s\n' \
                "${name}" "${expected}" >&2
            return 1
        fi
    done
}

vm_args=(
    run
    -p
    helios-inspector
    --
    vm
    --arch
    "${arch}"
    --cpu
    "${cpu}"
    --smp
    "${smp}"
)

if [[ "${HELIOS_WASI_SMOKE_RELEASE:-0}" == "1" ]]; then
    vm_args+=(--release)
fi

check_wasm_imports \
    dash \
    artifacts/wasix/dash/dash.wasm \
    '(import "wasix_32v1" "callback_signal"' \
    '(import "wasix_32v1" "thread_id"' \
    '(import "wasix_32v1" "thread_signal"' \
    '(import "wasix_32v1" "futex_wait"' \
    '(import "wasix_32v1" "futex_wake"' \
    '(import "wasix_32v1" "futex_wake_all"' \
    '(import "wasix_32v1" "thread_exit"' \
    '(import "wasix_32v1" "stack_checkpoint"' \
    '(import "wasix_32v1" "stack_restore"' \
    '(import "wasix_32v1" "proc_fork"' \
    '(import "wasix_32v1" "proc_exec"' \
    '(export "wasi_thread_start"'

check_wasm_imports \
    bash \
    artifacts/wasix/bash/bash.wasm \
    '(import "wasix_32v1" "callback_signal"' \
    '(import "wasix_32v1" "thread_id"' \
    '(import "wasix_32v1" "thread_signal"' \
    '(import "wasix_32v1" "futex_wait"' \
    '(import "wasix_32v1" "futex_wake"' \
    '(import "wasix_32v1" "futex_wake_all"' \
    '(import "wasix_32v1" "thread_exit"' \
    '(import "wasix_32v1" "stack_checkpoint"' \
    '(import "wasix_32v1" "stack_restore"' \
    '(import "wasix_32v1" "proc_fork"' \
    '(import "wasix_32v1" "proc_exec3"' \
    '(export "wasi_thread_start"'

build_wasix_conformance_artifact thread-futex
build_wasix_conformance_artifact continuation

run_smoke() {
    local name="$1"
    shift
    local expected=()
    while [[ "$#" -gt 0 && "$1" != "--" ]]; do
        expected+=("$1")
        shift
    done
    if [[ "$#" -eq 0 ]]; then
        printf 'smoke %s is missing command separator\n' "${name}" >&2
        return 1
    fi
    shift

    printf '==> %s\n' "${name}" >&2

    local output
    if ! output="$("${cargo_bin}" "${vm_args[@]}" "$@" 2>&1)"; then
        printf '%s\n' "${output}" >&2
        printf 'smoke %s failed before expected output check\n' "${name}" >&2
        return 1
    fi

    local item
    for item in "${expected[@]}"; do
        if ! grep -Fq -- "${item}" <<<"${output}"; then
            printf '%s\n' "${output}" >&2
            printf 'smoke %s did not produce expected output: %s\n' "${name}" "${item}" >&2
            return 1
        fi
    done
}

# The kernel seeds its wall clock from the platform real-time clock
# before any program runs, so a guest asking wasi:clocks/wall-clock must
# agree with the host that booted it. Comparing the printed epoch with
# the host clock is what proves the seed came from the device rather
# than from the kernel's own uptime, and it is the assertion here: the
# kernel's own "wall clock seeded" line only reaches the serial on a
# backend that mirrors kernel logs there, so it is reported as evidence
# when present rather than required.
run_wall_clock_smoke() {
    printf '==> %s\n' wall-clock >&2

    local output
    if ! output="$("${cargo_bin}" "${vm_args[@]}" \
        --boot-program dash \
        --boot-program debugger \
        --boot-program date \
        --no-compiler-plugin \
        shell -c '/bin/date' 2>&1)"; then
        printf '%s\n' "${output}" >&2
        printf 'smoke wall-clock failed before expected output check\n' >&2
        return 1
    fi

    local seed_line
    seed_line="$(grep -o -- 'wall clock seeded source=.*' <<<"${output}" | tail -n 1)"
    if [[ -n "${seed_line}" ]]; then
        printf '%s\n' "${seed_line}" >&2
    fi

    local guest_epoch
    guest_epoch="$(sed -n 's/^unix_seconds=\([0-9][0-9]*\).*$/\1/p' <<<"${output}" | tail -n 1)"
    if [[ -z "${guest_epoch}" ]]; then
        printf '%s\n' "${output}" >&2
        printf 'smoke wall-clock: the guest printed no unix_seconds line\n' >&2
        return 1
    fi

    local host_epoch skew
    host_epoch="$(date +%s)"
    skew=$(( guest_epoch > host_epoch ? guest_epoch - host_epoch : host_epoch - guest_epoch ))
    if (( skew > WALL_CLOCK_TOLERANCE_SECONDS )); then
        printf '%s\n' "${output}" >&2
        printf 'smoke wall-clock: guest epoch %s is %ss from host epoch %s\n' \
            "${guest_epoch}" "${skew}" "${host_epoch}" >&2
        return 1
    fi
    printf 'wall clock within %ss of the host (guest %s, host %s)\n' \
        "${skew}" "${guest_epoch}" "${host_epoch}" >&2
}

run_smoke \
    dash \
    dash:42 \
    -- \
    --boot-program dash \
    --boot-program debugger \
    --no-compiler-plugin \
    shell \
    -c \
    'echo dash:42'

run_wall_clock_smoke

run_smoke \
    dash-process \
    bg:ok \
    done:ok \
    sub:ok \
    pipe:ok \
    -- \
    --boot-program dash \
    --boot-program debugger \
    --boot-program cat \
    --boot-program mkdir \
    --no-compiler-plugin \
    shell \
    -c \
    '/bin/mkdir /dp; echo pipe:ok > /dp/in; /bin/dash -c "echo bg:ok & wait; echo done:ok; (echo sub:ok); /bin/cat /dp/in | /bin/cat"'

run_smoke \
    bash-coreutils \
    ok \
    HELIOS_SMOKE=pass \
    script:ok \
    cwd:/ \
    -- \
    --boot-program dash \
    --boot-program debugger \
    --boot-program bash \
    --boot-program mkdir \
    --boot-program cat \
    --boot-program env \
    --no-compiler-plugin \
    shell \
    -c \
    '/bin/bash -c "cd /; /bin/mkdir d; echo ok > /d/f; /bin/cat /d/f | /bin/cat; echo \"echo script:ok\" > /d/s; /bin/bash /d/s; /bin/env HELIOS_SMOKE=pass; echo cwd:$PWD"'

run_smoke \
    bash-exit-status \
    status:7 \
    -- \
    --boot-program dash \
    --boot-program debugger \
    --boot-program bash \
    --no-compiler-plugin \
    shell \
    -c \
    '/bin/bash -c "exit 7"; echo status:$?'

run_smoke \
    quickjs \
    42 \
    -- \
    --boot-program dash \
    --boot-program debugger \
    --boot-program quickjs \
    --no-compiler-plugin \
    shell \
    -c \
    '/bin/qjs -e "console.log(40+2)"'

run_smoke \
    wasix-thread-futex \
    thread-futex:ok \
    -- \
    --boot-program dash \
    --boot-program debugger \
    --boot-program wasix-thread-futex \
    --no-compiler-plugin \
    shell \
    -c \
    '/bin/wasix-thread-futex'

run_smoke \
    wasix-continuation \
    continuation:ok \
    -- \
    --boot-program dash \
    --boot-program debugger \
    --boot-program wasix-continuation \
    --no-compiler-plugin \
    shell \
    -c \
    '/bin/wasix-continuation'

run_smoke \
    cpython \
    '{"ok":"a"}' \
    -- \
    --boot-program dash \
    --boot-program debugger \
    --boot-program python3 \
    --no-compiler-plugin \
    shell \
    -c \
    '/bin/python3 -c "import json, pathlib; print(json.dumps({\"ok\": pathlib.PurePosixPath(\"/a\").name}, separators=(\",\",\":\")))"'

# The slirp gateway answers ICMP echo without leaving the host, so the
# ping path is covered even when the runner has no outbound network.
run_smoke \
    ping-gateway \
    'bytes from 10.0.2.2' \
    -- \
    --boot-program dash \
    --boot-program debugger \
    --boot-program ping \
    --no-compiler-plugin \
    shell \
    -c \
    '/bin/ping 10.0.2.2'

if [[ "${HELIOS_WASI_SMOKE_CURL:-0}" == "1" ]]; then
    run_smoke \
        ping-by-name \
        'bytes from ' \
        -- \
        --boot-program dash \
        --boot-program debugger \
        --boot-program ping \
        --no-compiler-plugin \
        shell \
        -c \
        '/bin/ping detectportal.firefox.com'

    # Launch by bootfs path: dash execs "/bin/curl" and the guest must see
    # argv[0] == "/bin/curl".
    run_smoke \
        curl-by-path \
        'success' \
        -- \
        --boot-program dash \
        --boot-program debugger \
        --boot-program curl \
        --boot-program http-client \
        --no-compiler-plugin \
        shell \
        -c \
        '/bin/curl http://detectportal.firefox.com/success.txt'

    # Launch by bare name: dash resolves PATH itself and execs "/bin/curl"
    # while naming the child "curl". The guest must see argv[0] == "curl"
    # exactly once (issue #35).
    run_smoke \
        curl-by-name \
        'success' \
        -- \
        --boot-program dash \
        --boot-program debugger \
        --boot-program curl \
        --boot-program http-client \
        --no-compiler-plugin \
        shell \
        -c \
        'curl http://detectportal.firefox.com/success.txt'
fi
