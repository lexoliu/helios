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
dash_source_url="${WASIX_DASH_SOURCE_URL:-https://wasmer.io/sharrattj/dash}"
dash_webc_url="${WASIX_DASH_WEBC_URL:-https://cdn.wasmer.io/webcimages/05e14719a38cc0cc84823ecabec48ca2b76f82187a7c4833bee82e5dbcbf4c30.webc}"
dash_webc_sha256="${WASIX_DASH_WEBC_SHA256:-05e14719a38cc0cc84823ecabec48ca2b76f82187a7c4833bee82e5dbcbf4c30}"
dash_wasm_sha256="${WASIX_DASH_WASM_SHA256:-ce4d85249665844ea9d882bda5efed67d127605735c7f5b674157804e624c9cc}"

bash_package="${WASIX_BASH_PACKAGE:-wasmer/bash}"
bash_version="${WASIX_BASH_VERSION:-1.0.25}"
bash_source_url="${WASIX_BASH_SOURCE_URL:-https://wasmer.io/wasmer/bash}"
bash_webc_url="${WASIX_BASH_WEBC_URL:-https://cdn.wasmer.io/webcimages/059606d132e2e6bc1afe3b432ee64dcb1b1b059815c8bb213cf3b24798ef21e1.webc}"
bash_webc_sha256="${WASIX_BASH_WEBC_SHA256:-059606d132e2e6bc1afe3b432ee64dcb1b1b059815c8bb213cf3b24798ef21e1}"
bash_wasm_sha256="${WASIX_BASH_WASM_SHA256:-5b37a9f95fca50f55024b7fc37b786fa80024e55320266cb19936729d24c70ce}"

quickjs_package="${QUICKJS_PACKAGE:-saghul/quickjs}"
quickjs_version="${QUICKJS_VERSION:-0.0.3}"
quickjs_source_url="${QUICKJS_SOURCE_URL:-https://wasmer.io/_/quickjs}"
quickjs_webc_url="${QUICKJS_WEBC_URL:-https://cdn.wasmer.io/webcimages/db5822e7db5e982ac387a588649d260f64d20d89e6feecfcc1ef7acbccafe9d6.webc}"
quickjs_webc_sha256="${QUICKJS_WEBC_SHA256:-db5822e7db5e982ac387a588649d260f64d20d89e6feecfcc1ef7acbccafe9d6}"
quickjs_wasm_sha256="${QUICKJS_WASM_SHA256:-6f62a6bc5c8f8e3e12a54e2ecbc5674ccfe1c75f91d8e4dd6ebb3fec422a4d6c}"

coreutils_package="${COREUTILS_PACKAGE:-wasmer/coreutils}"
coreutils_version="${COREUTILS_VERSION:-1.0.19}"
coreutils_source_url="${COREUTILS_SOURCE_URL:-https://wasmer.io/wasmer/coreutils}"
coreutils_webc_url="${COREUTILS_WEBC_URL:-https://cdn.wasmer.io/webcimages/6b2fd4494bd198f60859987608a7633f807a05147e4d8398ec061639d047ce75.webc}"
coreutils_webc_sha256="${COREUTILS_WEBC_SHA256:-6b2fd4494bd198f60859987608a7633f807a05147e4d8398ec061639d047ce75}"
coreutils_wasm_sha256="${COREUTILS_WASM_SHA256:-586489a3aca13cb69855f9f92e655acdb1598934b24ceb9450628d0ab34d0b95}"

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT

sha256_hex() {
  shasum -a 256 "$1" | cut -d' ' -f1
}

verify_expected_sha256() {
  local path="$1"
  local expected="$2"
  local actual
  actual="$(sha256_hex "$path")"
  if [[ "$actual" != "$expected" ]]; then
    printf 'sha256 mismatch for %s: expected %s, got %s\n' \
      "$path" "$expected" "$actual" >&2
    exit 1
  fi
}

stage_wasmer_webc_atom() {
  local label="$1"
  local raw_wasm_source="$2"
  local webc_source="$3"
  local webc_url="$4"
  local webc_sha256="$5"
  local wasm_sha256="$6"
  local output="$7"

  if [[ -n "$raw_wasm_source" ]]; then
    cp -f "$raw_wasm_source" "$output"
  else
    local webc_path="$webc_source"
    if [[ -z "$webc_path" ]]; then
      webc_path="$staging/$label.webc"
      echo "Downloading $label WEBc image..."
      curl -fL -o "$webc_path" "$webc_url"
    fi
    verify_expected_sha256 "$webc_path" "$webc_sha256"
    "$repo_root/tools/wasi-apps/extract-webc-wasm.pl" "$webc_path" "$output"
  fi

  wasm-tools validate "$output"
  verify_expected_sha256 "$output" "$wasm_sha256"
}

write_source_record() {
  local output="$1"
  local package="$2"
  local version="$3"
  local source_url="$4"
  local wasm_sha256="$5"
  local webc_sha256="${6:-}"

  {
    printf 'package=%s\n' "$package"
    printf 'version=%s\n' "$version"
    printf 'source=%s\n' "$source_url"
    if [[ -n "$webc_sha256" ]]; then
      printf 'webc_sha256=%s\n' "$webc_sha256"
    fi
    printf 'sha256=%s\n' "$wasm_sha256"
  } > "$output"
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
sha256sum "$python_root/python3.wasm" > "$python_root/python3.wasm.sha256"

cp -r "$staging/cpython/lib" "$python_root/"

echo "CPython installed at: $python_root"
ls -lh "$python_root/python3.wasm" "$python_root/python3.wasm.sha256"

echo "Staging WASIX dash $dash_package@$dash_version..."
stage_wasmer_webc_atom \
  "dash-$dash_version" \
  "${WASIX_DASH_WASM:-}" \
  "${WASIX_DASH_WEBC:-}" \
  "$dash_webc_url" \
  "$dash_webc_sha256" \
  "$dash_wasm_sha256" \
  "$dash_root/dash.wasm"
sha256sum "$dash_root/dash.wasm" > "$dash_root/dash.wasm.sha256"
write_source_record \
  "$dash_root/SOURCE.txt" \
  "$dash_package" \
  "$dash_version" \
  "$dash_source_url" \
  "$dash_wasm_sha256" \
  "$dash_webc_sha256"

echo "WASIX dash installed at: $dash_root/dash.wasm"
ls -lh "$dash_root/dash.wasm" "$dash_root/dash.wasm.sha256"

echo "Staging WASIX bash $bash_package@$bash_version..."
stage_wasmer_webc_atom \
  "bash-$bash_version" \
  "${WASIX_BASH_WASM:-}" \
  "${WASIX_BASH_WEBC:-}" \
  "$bash_webc_url" \
  "$bash_webc_sha256" \
  "$bash_wasm_sha256" \
  "$bash_root/bash.wasm"
sha256sum "$bash_root/bash.wasm" > "$bash_root/bash.wasm.sha256"
write_source_record \
  "$bash_root/SOURCE.txt" \
  "$bash_package" \
  "$bash_version" \
  "$bash_source_url" \
  "$bash_wasm_sha256" \
  "$bash_webc_sha256"

echo "WASIX bash installed at: $bash_root/bash.wasm"
ls -lh "$bash_root/bash.wasm" "$bash_root/bash.wasm.sha256"

echo "Staging QuickJS $quickjs_package@$quickjs_version..."
stage_wasmer_webc_atom \
  "quickjs-$quickjs_version" \
  "${QUICKJS_WASM:-}" \
  "${QUICKJS_WEBC:-}" \
  "$quickjs_webc_url" \
  "$quickjs_webc_sha256" \
  "$quickjs_wasm_sha256" \
  "$quickjs_root/qjs.wasm"
sha256sum "$quickjs_root/qjs.wasm" > "$quickjs_root/qjs.wasm.sha256"
write_source_record \
  "$quickjs_root/SOURCE.txt" \
  "$quickjs_package" \
  "$quickjs_version" \
  "$quickjs_source_url" \
  "$quickjs_wasm_sha256" \
  "$quickjs_webc_sha256"

echo "QuickJS installed at: $quickjs_root/qjs.wasm"
ls -lh "$quickjs_root/qjs.wasm" "$quickjs_root/qjs.wasm.sha256"

echo "Staging coreutils $coreutils_package@$coreutils_version..."
stage_wasmer_webc_atom \
  "coreutils-$coreutils_version" \
  "${COREUTILS_WASM:-}" \
  "${COREUTILS_WEBC:-}" \
  "$coreutils_webc_url" \
  "$coreutils_webc_sha256" \
  "$coreutils_wasm_sha256" \
  "$coreutils_root/coreutils.wasm"
sha256sum "$coreutils_root/coreutils.wasm" > "$coreutils_root/coreutils.wasm.sha256"
write_source_record \
  "$coreutils_root/SOURCE.txt" \
  "$coreutils_package" \
  "$coreutils_version" \
  "$coreutils_source_url" \
  "$coreutils_wasm_sha256" \
  "$coreutils_webc_sha256"

echo "coreutils installed at: $coreutils_root/coreutils.wasm"
ls -lh "$coreutils_root/coreutils.wasm" "$coreutils_root/coreutils.wasm.sha256"

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
