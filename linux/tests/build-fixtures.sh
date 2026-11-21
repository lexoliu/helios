#!/usr/bin/env bash
# Builds a boot root holding the static Linux fixtures of one architecture
# under /linux beside init's own files, for `helios-inspector vm
# --bootfs-root`.
# Usage: build-fixtures.sh <output directory> [x86_64|aarch64]
set -euo pipefail
cd "$(dirname "$0")/../.."
out="${1:?bootfs output directory required}"
arch="${2:-x86_64}"
case "$arch" in
    x86_64) sources=linux/tests ;;
    aarch64) sources=linux/tests/aarch64 ;;
    *) echo "unknown fixture architecture: $arch" >&2; exit 1 ;;
esac
mkdir -p "$out/linux"
cp -R programs/init/bootfs/. "$out/"
for program in hello fault spin; do
    zig cc -target "$arch-linux-musl" -nostdlib -static -no-pie -Wl,--build-id=none \
        "$sources/$program.S" -o "$out/linux/$program"
done
# `hello-libc` and `args` exercise startup and descriptors, `memory` the
# memory calls; the rest are the system-information suite, one program per
# call group, each checking its calls' results and errors against Linux's
# behaviour.
for program in hello-libc args memory clock random identity limits machine priority pthreads; do
    zig cc -target "$arch-linux-musl" -static -no-pie -s -O2 -Wl,--build-id=none \
        -Ilinux/tests "linux/tests/$program.c" -o "$out/linux/$program"
done
cp linux/tests/check-memory.sh "$out/linux/check-memory.sh"
cp linux/tests/check-pthreads.sh "$out/linux/check-pthreads.sh"
# The same executables first run under Linux, establishing their ABI assertions.
if [[ "$(uname -s)" == Linux && "$(uname -m)" == "$arch" ]]; then
    "$out/linux/hello"
    "$out/linux/hello-libc"
    HELIOS_PROCESS_ID=1 "$out/linux/args" 0 >/dev/null
    sh "$out/linux/check-memory.sh" "$out/linux/memory"
    sh "$out/linux/check-pthreads.sh" "$out/linux/pthreads"
    # Calls that need a privilege are asserted only for a caller known to
    # lack it; a privileged one may succeed or fail, and the fixture checks
    # either answer.
    if [[ "$(id -u)" != 0 ]]; then
        "$out/linux/clock" denied
        "$out/linux/limits" unprivileged
        "$out/linux/priority" unprivileged
    else
        "$out/linux/clock"
        "$out/linux/limits"
        "$out/linux/priority"
    fi
    "$out/linux/random"
    "$out/linux/identity"
    "$out/linux/machine"
fi
