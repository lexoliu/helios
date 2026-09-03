#!/usr/bin/env bash
#
# Cross-builds the Linux-native benchmark counterparts for one guest
# architecture with the zig toolchain CI already carries, as static musl
# binaries so the pinned Fedora guest needs no build tools to run them.
#
#   tools/bench/native/build.sh <aarch64|x86_64> [out-dir]
#
# Output: <out-dir>/<arch>/<name> for every tool, default
# artifacts/bench-native/<arch>/.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
native_root="$repo_root/tools/bench/native"

arch="${1:-}"
case "$arch" in
  aarch64|x86_64) ;;
  *)
    printf 'usage: %s <aarch64|x86_64> [out-dir]\n' "$0" >&2
    exit 2
    ;;
esac
out_root="${2:-$repo_root/artifacts/bench-native}"
out_dir="$out_root/$arch"
mkdir -p "$out_dir"

if ! command -v zig >/dev/null; then
  printf 'required tool missing: zig\n' >&2
  exit 1
fi

tools=(hello pipe-echo hostcall-loop tcp-latency sched-tasks procbench)
for tool in "${tools[@]}"; do
  zig cc \
    -target "$arch-linux-musl" \
    -O2 -static -std=c11 -Wall -Wextra -Werror \
    -D_GNU_SOURCE \
    -o "$out_dir/$tool" \
    "$native_root/$tool.c"
done

printf 'native benchmark counterparts written to %s\n' "$out_dir"
ls -l "$out_dir"
