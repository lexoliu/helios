#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out_dir="${1:-$repo_root/artifacts/wasi-tools}"

mkdir -p "$out_dir"

build_env=(
  CARGO_PROFILE_DEV_OPT_LEVEL=z
  CARGO_PROFILE_DEV_DEBUG=0
  CARGO_PROFILE_DEV_CODEGEN_UNITS=1
  RUSTFLAGS='-C debuginfo=0 -C strip=debuginfo'
)

env "${build_env[@]}" cargo build \
  --manifest-path "$repo_root/tools/wasi-apps/python/Cargo.toml" \
  --target wasm32-wasip2

cp -f \
  "$repo_root/tools/wasi-apps/python/target/wasm32-wasip2/debug/helios-python-wasi.wasm" \
  "$out_dir/python.wasm"

wasm-tools strip "$out_dir/python.wasm" -o "$out_dir/python-stripped.wasm"

env "${build_env[@]}" cargo build \
  --manifest-path "$repo_root/tools/wasi-apps/curl/Cargo.toml" \
  --target wasm32-wasip2

cp -f \
  "$repo_root/tools/wasi-apps/curl/target/wasm32-wasip2/debug/helios_curl_wasi.wasm" \
  "$out_dir/curl.wasm"

wasm-tools strip "$out_dir/curl.wasm" -o "$out_dir/curl-stripped.wasm"

echo "wasi artifacts written to: $out_dir"
ls -lh "$out_dir"/python*.wasm "$out_dir"/curl*.wasm
