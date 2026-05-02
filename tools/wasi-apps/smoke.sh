#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"

cargo_bin="${CARGO:-cargo}"
arch="${HELIOS_WASI_SMOKE_ARCH:-aarch64}"
cpu="${HELIOS_WASI_SMOKE_CPU:-max}"
smp="${HELIOS_WASI_SMOKE_SMP:-1}"
tmp_files=()

cleanup() {
    rm -f "${tmp_files[@]}"
}

trap cleanup EXIT

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

run_smoke \
    dash-process \
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
    '/bin/mkdir /dp; echo pipe:ok > /dp/in; /bin/dash -c "(echo sub:ok); /bin/cat /dp/in | /bin/cat"'

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

if [[ "${HELIOS_WASI_SMOKE_CURL:-0}" == "1" ]]; then
    run_smoke \
        curl \
        '<title>Example Domain</title>' \
        -- \
        --boot-program dash \
        --boot-program debugger \
        --boot-program curl \
        --no-compiler-plugin \
        shell \
        -c \
        '/bin/curl http://example.com/'
fi
