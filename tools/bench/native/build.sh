#!/usr/bin/env bash
#
# Cross-builds the Linux-native benchmark counterparts for one guest
# architecture with the zig toolchain CI already carries: the C tools as
# static musl binaries, so the pinned Fedora guest needs no build tools to
# run them, and `helios-compiler-host` (compiler-host/) as a glibc binary
# through cargo-zigbuild, so the compiler plugin it hosts allocates through
# the guest's own glibc malloc, as a stock Wasmtime on Linux does.
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

for required in zig cargo-zigbuild; do
  if ! command -v "$required" >/dev/null; then
    printf 'required tool missing: %s\n' "$required" >&2
    exit 1
  fi
done

tools=(hello pipe-echo hostcall-loop tcp-latency sched-tasks procbench)
for tool in "${tools[@]}"; do
  zig cc \
    -target "$arch-linux-musl" \
    -O2 -static -std=c11 -Wall -Wextra -Werror \
    -D_GNU_SOURCE \
    -o "$out_dir/$tool" \
    "$native_root/$tool.c"
done

# A release build, as every benchmark artifact is (AGENTS.md §3.4).
rust_target="$arch-unknown-linux-gnu"
(
  cd "$repo_root"
  cargo zigbuild --release --locked -p helios-compiler-host --target "$rust_target"
)
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
cp "$target_dir/$rust_target/release/helios-compiler-host" "$out_dir/helios-compiler-host"

printf 'native benchmark counterparts written to %s\n' "$out_dir"
ls -l "$out_dir"
