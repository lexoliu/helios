#!/usr/bin/env bash
#
# Build helios's WASI side tooling.
#
# - Downloads the official (unofficial-but-canonical) CPython WASI build
#   from `brettcannon/cpython-wasi-build`, runs it through the wasi
#   preview1→p2 adapter shipped alongside wasmtime, and stashes the
#   component + stdlib under `$out_dir/../python3-root`.
# - Builds our Rust `curl-wasi` from source.
# - Stages the standard WASIX dash artifact used as `/bin/dash` in the
#   boot filesystem.
#
# Network-gated: the CPython download needs internet; pass a pre-staged
# zip via `CPYTHON_WASI_ZIP=<path>` to skip the curl step. The dash
# artifact is provenance-gated; pass the raw official module via
# `WASIX_DASH_WASM=<path>`.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
artifacts_root="${ARTIFACTS_ROOT:-$repo_root/artifacts}"
out_dir="${1:-$artifacts_root/wasi-tools}"
python_root="$artifacts_root/python3-root"
dash_root="$artifacts_root/wasix/dash"

mkdir -p "$out_dir" "$python_root" "$dash_root"

cpython_version="${CPYTHON_VERSION:-3.14.4}"
wasmtime_version="${WASMTIME_ADAPTER_VERSION:-43.0.1}"
dash_package="${WASIX_DASH_PACKAGE:-sharrattj/dash}"
dash_version="${WASIX_DASH_VERSION:-1.0.19}"
dash_source_url="${WASIX_DASH_SOURCE_URL:-https://wasmer.io/sharrattj/dash}"

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
python_component_raw="$staging/python3.component.wasm"
wasm-tools component new \
  "$staging/cpython/python.wasm" \
  --adapt "$adapter_path" \
  -o "$python_component_raw"
wasm-tools strip "$python_component_raw" -o "$python_root/python3.wasm"
sha256sum "$python_root/python3.wasm" > "$python_root/python3.wasm.sha256"

cp -r "$staging/cpython/lib" "$python_root/"

echo "CPython installed at: $python_root"
ls -lh "$python_root/python3.wasm" "$python_root/python3.wasm.sha256"

dash_wasm_source="${WASIX_DASH_WASM:-}"
if [[ -z "$dash_wasm_source" ]]; then
  printf '%s\n' \
    "WASIX_DASH_WASM is required to stage /bin/dash." \
    "Use the official Wasmer package $dash_package@$dash_version from $dash_source_url," \
    "extract its raw dash wasm module, then rerun with WASIX_DASH_WASM=/path/to/dash.wasm." >&2
  exit 1
fi

echo "Staging WASIX dash $dash_package@$dash_version..."
cp -f "$dash_wasm_source" "$dash_root/dash.wasm"
wasm-tools validate "$dash_root/dash.wasm"
sha256sum "$dash_root/dash.wasm" > "$dash_root/dash.wasm.sha256"
{
  printf 'package=%s\n' "$dash_package"
  printf 'version=%s\n' "$dash_version"
  printf 'source=%s\n' "$dash_source_url"
  printf 'sha256='
  cut -d' ' -f1 "$dash_root/dash.wasm.sha256"
} > "$dash_root/SOURCE.txt"

echo "WASIX dash installed at: $dash_root/dash.wasm"
ls -lh "$dash_root/dash.wasm" "$dash_root/dash.wasm.sha256"

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
sha256sum "$out_dir/curl.wasm" > "$out_dir/curl.wasm.sha256"

wasm-tools strip "$out_dir/curl.wasm" -o "$out_dir/curl-stripped.wasm"
sha256sum "$out_dir/curl-stripped.wasm" > "$out_dir/curl-stripped.wasm.sha256"

echo "wasi artifacts written to: $out_dir and $python_root"
ls -lh "$out_dir"/curl*.wasm
