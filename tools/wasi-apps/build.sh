#!/usr/bin/env bash
#
# Build helios's WASI side tooling.
#
# - Downloads the official (unofficial-but-canonical) CPython WASI build
#   from `brettcannon/cpython-wasi-build`, runs it through the wasi
#   preview1→p2 adapter shipped alongside wasmtime, and stashes the
#   component + stdlib under `$out_dir/../python3-root`.
# - Builds our Rust `curl-wasi` from source.
#
# Network-gated: the CPython download needs internet; pass a pre-staged
# zip via `CPYTHON_WASI_ZIP=<path>` to skip the curl step.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
artifacts_root="${ARTIFACTS_ROOT:-$repo_root/artifacts}"
out_dir="${1:-$artifacts_root/wasi-tools}"
python_root="$artifacts_root/python3-root"

mkdir -p "$out_dir" "$python_root"

cpython_version="${CPYTHON_VERSION:-3.14.4}"
wasmtime_version="${WASMTIME_ADAPTER_VERSION:-43.0.1}"

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT

zip_path="${CPYTHON_WASI_ZIP:-}"
if [[ -z "$zip_path" ]]; then
  zip_path="$staging/cpython.zip"
  echo "Downloading CPython $cpython_version WASI build..."
  curl -fL -o "$zip_path" \
    "https://github.com/brettcannon/cpython-wasi-build/releases/download/v${cpython_version}/python-${cpython_version}-wasi_sdk-24.zip"
fi

adapter_path="$staging/wasi_snapshot_preview1.command.wasm"
echo "Downloading wasmtime $wasmtime_version preview1 adapter..."
curl -fL -o "$adapter_path" \
  "https://github.com/bytecodealliance/wasmtime/releases/download/v${wasmtime_version}/wasi_snapshot_preview1.command.wasm"

echo "Extracting CPython..."
rm -rf "$python_root/lib" "$python_root/python3.wasm"
mkdir -p "$python_root"
unzip -q "$zip_path" -d "$staging/cpython"

echo "Converting python.wasm to a WASI P2 component..."
wasm-tools component new \
  "$staging/cpython/python.wasm" \
  --adapt "$adapter_path" \
  -o "$python_root/python3.wasm"

cp -r "$staging/cpython/lib" "$python_root/"

echo "CPython installed at: $python_root"
ls -lh "$python_root/python3.wasm"

build_env=(
  CARGO_PROFILE_DEV_OPT_LEVEL=z
  CARGO_PROFILE_DEV_DEBUG=0
  CARGO_PROFILE_DEV_CODEGEN_UNITS=1
  RUSTFLAGS='-C debuginfo=0 -C strip=debuginfo'
)

env "${build_env[@]}" cargo build \
  --manifest-path "$repo_root/tools/wasi-apps/curl/Cargo.toml" \
  --target wasm32-wasip2

cp -f \
  "$repo_root/tools/wasi-apps/curl/target/wasm32-wasip2/debug/helios_curl_wasi.wasm" \
  "$out_dir/curl.wasm"

wasm-tools strip "$out_dir/curl.wasm" -o "$out_dir/curl-stripped.wasm"

echo "wasi artifacts written to: $out_dir and $python_root"
ls -lh "$out_dir"/curl*.wasm
