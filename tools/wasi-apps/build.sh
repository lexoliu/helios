#!/usr/bin/env bash
#
# Build helios's WASI side tooling.
#
# - Downloads the official (unofficial-but-canonical) CPython WASI build
#   from `brettcannon/cpython-wasi-build`, runs it through the wasi
#   preview1→p2 adapter shipped alongside wasmtime, and stashes the
#   component + stdlib under `$out_dir/../python3-root`.
# - Builds our Rust `curl-wasi` from source.
# - Stages standard Wasmer WASIX/WASI shell and language artifacts used by
#   the boot filesystem.
#
# Network-gated: the CPython download needs internet; pass a pre-staged
# zip via `CPYTHON_WASI_ZIP=<path>` to skip the curl step. Wasmer
# artifacts may be supplied as raw modules (`*_WASM=<path>`) or WEBc
# images (`*_WEBC=<path>`); otherwise this script downloads the pinned
# official WEBc images and extracts their wasm atoms.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
artifacts_root="${ARTIFACTS_ROOT:-$repo_root/artifacts}"
out_dir="${1:-$artifacts_root/wasi-tools}"
python_root="$artifacts_root/python3-root"
dash_root="$artifacts_root/wasix/dash"
bash_root="$artifacts_root/wasix/bash"
quickjs_root="$artifacts_root/wasix/quickjs"
coreutils_root="$artifacts_root/wasix/coreutils"

mkdir -p "$out_dir" "$python_root" "$dash_root" "$bash_root" "$quickjs_root" "$coreutils_root"

cpython_version="${CPYTHON_VERSION:-3.14.4}"
wasmtime_version="${WASMTIME_ADAPTER_VERSION:-43.0.1}"
dash_package="${WASIX_DASH_PACKAGE:-sharrattj/dash}"
dash_version="${WASIX_DASH_VERSION:-1.0.19}"
dash_webc_url="${WASIX_DASH_WEBC_URL:-https://cdn.wasmer.io/webcimages/05e14719a38cc0cc84823ecabec48ca2b76f82187a7c4833bee82e5dbcbf4c30.webc}"

bash_package="${WASIX_BASH_PACKAGE:-wasmer/bash}"
bash_version="${WASIX_BASH_VERSION:-1.0.25}"
bash_webc_url="${WASIX_BASH_WEBC_URL:-https://cdn.wasmer.io/webcimages/059606d132e2e6bc1afe3b432ee64dcb1b1b059815c8bb213cf3b24798ef21e1.webc}"

quickjs_package="${QUICKJS_PACKAGE:-saghul/quickjs}"
quickjs_version="${QUICKJS_VERSION:-0.0.3}"
quickjs_webc_url="${QUICKJS_WEBC_URL:-https://cdn.wasmer.io/webcimages/db5822e7db5e982ac387a588649d260f64d20d89e6feecfcc1ef7acbccafe9d6.webc}"

coreutils_package="${COREUTILS_PACKAGE:-wasmer/coreutils}"
coreutils_version="${COREUTILS_VERSION:-1.0.19}"
coreutils_webc_url="${COREUTILS_WEBC_URL:-https://cdn.wasmer.io/webcimages/6b2fd4494bd198f60859987608a7633f807a05147e4d8398ec061639d047ce75.webc}"

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT

verify_wasm_uses_simd() {
  local path="$1"
  if ! wasm-tools print "$path" | grep -Eq '(^|[[:space:]()])(v128|i8x16|i16x8|i32x4|i64x2|f32x4|f64x2)\.'; then
    printf 'wasm SIMD instructions missing from %s\n' "$path" >&2
    exit 1
  fi
}

stage_wasmer_webc_atom() {
  local label="$1"
  local raw_wasm_source="$2"
  local webc_source="$3"
  local webc_url="$4"
  local output="$5"

  if [[ -n "$raw_wasm_source" ]]; then
    cp -f "$raw_wasm_source" "$output"
  else
    local webc_path="$webc_source"
    if [[ -z "$webc_path" ]]; then
      webc_path="$staging/$label.webc"
      echo "Downloading $label WEBc image..."
      curl -fL -o "$webc_path" "$webc_url"
    fi
    "$repo_root/tools/wasi-apps/extract-webc-wasm.pl" "$webc_path" "$output"
  fi

  wasm-tools validate "$output"
}

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

cp -r "$staging/cpython/lib" "$python_root/"

echo "CPython installed at: $python_root"
ls -lh "$python_root/python3.wasm"

echo "Staging WASIX dash $dash_package@$dash_version..."
stage_wasmer_webc_atom \
  "dash-$dash_version" \
  "${WASIX_DASH_WASM:-}" \
  "${WASIX_DASH_WEBC:-}" \
  "$dash_webc_url" \
  "$dash_root/dash.wasm"

echo "WASIX dash installed at: $dash_root/dash.wasm"
ls -lh "$dash_root/dash.wasm"

echo "Staging WASIX bash $bash_package@$bash_version..."
stage_wasmer_webc_atom \
  "bash-$bash_version" \
  "${WASIX_BASH_WASM:-}" \
  "${WASIX_BASH_WEBC:-}" \
  "$bash_webc_url" \
  "$bash_root/bash.wasm"

echo "WASIX bash installed at: $bash_root/bash.wasm"
ls -lh "$bash_root/bash.wasm"

echo "Staging QuickJS $quickjs_package@$quickjs_version..."
stage_wasmer_webc_atom \
  "quickjs-$quickjs_version" \
  "${QUICKJS_WASM:-}" \
  "${QUICKJS_WEBC:-}" \
  "$quickjs_webc_url" \
  "$quickjs_root/qjs.wasm"

echo "QuickJS installed at: $quickjs_root/qjs.wasm"
ls -lh "$quickjs_root/qjs.wasm"

echo "Staging coreutils $coreutils_package@$coreutils_version..."
stage_wasmer_webc_atom \
  "coreutils-$coreutils_version" \
  "${COREUTILS_WASM:-}" \
  "${COREUTILS_WEBC:-}" \
  "$coreutils_webc_url" \
  "$coreutils_root/coreutils.wasm"

echo "coreutils installed at: $coreutils_root/coreutils.wasm"
ls -lh "$coreutils_root/coreutils.wasm"

echo "Building Helios WASIX conformance artifacts..."
for test_name in thread-futex continuation simd-lanes; do
  test_root="$artifacts_root/wasix/$test_name"
  mkdir -p "$test_root"
  wasm-tools parse \
    "$repo_root/tools/wasi-apps/wasix-tests/$test_name.wat" \
    -o "$test_root/$test_name.wasm"
  wasm-tools validate "$test_root/$test_name.wasm"
  if [[ "$test_name" == "simd-lanes" ]]; then
    verify_wasm_uses_simd "$test_root/$test_name.wasm"
  fi
done

echo "WASIX conformance artifacts installed at: $artifacts_root/wasix"
ls -lh "$artifacts_root"/wasix/{thread-futex,continuation,simd-lanes}/*.wasm

build_env=(
  CARGO_PROFILE_RELEASE_OPT_LEVEL=3
  CARGO_PROFILE_RELEASE_DEBUG=0
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
  CARGO_PROFILE_RELEASE_LTO=fat
  RUSTFLAGS='-C debuginfo=0 -C strip=debuginfo'
)

env "${build_env[@]}" cargo build \
  --manifest-path "$repo_root/tools/wasi-apps/curl/Cargo.toml" \
  --target wasm32-wasip2 \
  --release

cp -f \
  "$repo_root/tools/wasi-apps/curl/target/wasm32-wasip2/release/helios_curl_wasi.wasm" \
  "$out_dir/curl.wasm"

wasm-tools strip "$out_dir/curl.wasm" -o "$out_dir/curl-stripped.wasm"

echo "wasi artifacts written to: $out_dir and $python_root"
ls -lh "$out_dir"/curl*.wasm
