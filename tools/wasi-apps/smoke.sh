#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"

cargo_bin="${CARGO:-cargo}"
arch="${HELIOS_WASI_SMOKE_ARCH:-aarch64}"
cpu="${HELIOS_WASI_SMOKE_CPU:-max}"
smp="${HELIOS_WASI_SMOKE_SMP:-1}"

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

run_smoke() {
    local name="$1"
    local expected="$2"
    shift 2

    printf '==> %s\n' "${name}" >&2

    local output
    if ! output="$("${cargo_bin}" "${vm_args[@]}" "$@" 2>&1)"; then
        printf '%s\n' "${output}" >&2
        printf 'smoke %s failed before expected output check\n' "${name}" >&2
        return 1
    fi

    if ! grep -Fq -- "${expected}" <<<"${output}"; then
        printf '%s\n' "${output}" >&2
        printf 'smoke %s did not produce expected output: %s\n' "${name}" "${expected}" >&2
        return 1
    fi
}

run_smoke \
    dash \
    dash:42 \
    --boot-program dash \
    --boot-program debugger \
    --no-compiler-plugin \
    shell \
    -c \
    'echo dash:42'

run_smoke \
    bash-coreutils \
    ok \
    --boot-program dash \
    --boot-program debugger \
    --boot-program bash \
    --boot-program mkdir \
    --boot-program cat \
    --no-compiler-plugin \
    shell \
    -c \
    '/bin/bash -c "cd /; /bin/mkdir d; echo ok > /d/f; /bin/cat /d/f"'

run_smoke \
    quickjs \
    42 \
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
        --boot-program dash \
        --boot-program debugger \
        --boot-program curl \
        --no-compiler-plugin \
        shell \
        -c \
        '/bin/curl http://example.com/'
fi
