# WASI/WASIX Python, Curl, And Dash On Helios

This document captures the exact workflow to stage and run `python`,
`curl`, and the default `/bin/dash` shell artifact. `dash`, CPython, and
`curl` are boot filesystem artifacts recorded in
`tools/wasi-apps/boot-artifacts.toml`.

## Build Artifacts

From repository root:

```bash
tools/wasi-apps/build.sh
```

This script:

1. Downloads the official CPython 3.14.4 WASI build
   (`brettcannon/cpython-wasi-build`) and the wasmtime
   `wasi_snapshot_preview1.command.wasm` adapter.
2. Wraps the preview1 core module into a WASI P2 component via
   `wasm-tools component new --adapt …`.
3. Installs `python3.wasm` plus `lib/python3.14/` (the CPython stdlib)
   under `artifacts/python3-root/`.
4. Stages the standard WASIX dash package `sharrattj/dash` version
   `1.0.19` from <https://wasmer.io/sharrattj/dash> as
   `artifacts/wasix/dash/dash.wasm`, validates it with `wasm-tools`,
   and records `artifacts/wasix/dash/dash.wasm.sha256` plus
   `artifacts/wasix/dash/SOURCE.txt`.
5. Builds the helios `curl-wasi` program from source into
   `artifacts/wasi-tools/`.

Artifacts produced:

- `artifacts/python3-root/python3.wasm` — real CPython 3.14 component.
- `artifacts/python3-root/lib/python3.14/` — CPython standard library.
- `artifacts/wasix/dash/dash.wasm` — standard WASIX dash raw module.
- `artifacts/wasix/dash/dash.wasm.sha256` — checksum consumed by
  `helios-cli kernel-prebuild`.
- `artifacts/wasix/dash/SOURCE.txt` — source package, version, URL, and
  checksum record.
- `artifacts/wasi-tools/curl.wasm`
- `artifacts/wasi-tools/curl-stripped.wasm`

The CPython download requires network. To re-stage in an offline
environment, place a pre-downloaded
`python-${VERSION}-wasi_sdk-24.zip` somewhere and pass its path via
`CPYTHON_WASI_ZIP=/path/to/zip tools/wasi-apps/build.sh`.

The dash artifact must be the official Wasmer package payload, not a
repo-local shell. In an offline environment, extract the raw module from
`sharrattj/dash` version `1.0.19` once and pass it explicitly:

```bash
WASIX_DASH_WASM=/path/to/official/dash.wasm tools/wasi-apps/build.sh
```

`helios-cli kernel-prebuild` verifies the checksum sidecar before adding
the artifact to bootfs. A missing or mismatched dash artifact is a hard
error; there is no repo-local fallback.

## Run In Helios (AArch64 HVF VM)

The fastest local VM path on Apple Silicon is AArch64 with HVF. Inspector
injects a host entropy seed through QEMU `fw_cfg`; the AArch64 backend
turns that platform seed into the kernel entropy source before WASI
`random_get` is exposed to CPython.

```bash
cargo run -p helios-inspector -- vm --arch aarch64 --cpu max --smp 1 \
  --boot-program dash --boot-program debugger --boot-program python3 \
  --no-compiler-plugin \
  shell -c '/bin/python3 -c "print(40+2)"'
```

Expected output includes:

```text
42
```

The same AArch64/HVF path is the preferred smoke test for the bootfs
`curl` artifact and kernel socket stack:

```bash
cargo run -p helios-inspector -- vm --arch aarch64 --release \
  --boot-program dash --boot-program debugger --boot-program curl \
  --no-compiler-plugin \
  shell -c '/bin/curl http://example.com/'
```

Expected output contains:

- `<!doctype html>`
- `<title>Example Domain</title>`

## Run In Helios (RISC-V VM)

Use inspector `vm` mode when checking RISC-V-specific behavior. Use
`--release` when the VM command should rebuild the guest kernel plus
embedded user-space programs (`init`, `debugger`) in release mode before
boot. CPython needs substantial guest memory; pass `--memory 2G`.

```bash
cargo run -p helios-inspector -- vm --arch riscv64 --memory 2G \
  --boot-program dash --boot-program debugger --boot-program python3 \
  --no-compiler-plugin \
  shell -c '/bin/python3 -c "print(40+2)"'
```

Expected output includes:

```text
42
```

```bash
cargo run -p helios-inspector -- vm --arch riscv64 --memory 2G \
  --boot-program dash --boot-program debugger --boot-program curl \
  --no-compiler-plugin \
  shell -c '/bin/curl http://example.com/'
```

Expected output contains:

- `<!doctype html>`
- `<title>Example Domain</title>`

## Implementation Notes

- `artifacts/python3-root/python3.wasm` is the real CPython interpreter
  converted from WASI preview1 to the preview2 component we run. In bootfs
  the stdlib is mounted under `usr/local/lib/python3.14/`, matching the
  upstream CPython build prefix.
- `tools/wasi-apps/curl` uses `helios-api::net::TcpStream` (WIT syscall
  surface) instead of host libc sockets, so it runs correctly inside
  helios. It currently supports `http://` URLs only.

## Contract For Future Agents

- Do not remove `tools/wasi-apps/build.sh` or rename output paths
  (`artifacts/python3-root/…`, `artifacts/wasix/dash/…`,
  `artifacts/wasi-tools/…`) without updating this document and
  `README.md`.
- Keep runtime behavior verified with the inspector `vm` commands above.
- Do not reintroduce a repo-local dash crate or shell stub. The boot
  shell is the standard WASIX dash artifact declared in
  `tools/wasi-apps/boot-artifacts.toml`.
- Do not reintroduce the old `tools/wasi-apps/python` stub — it was a
  Rust-level evalexpr toy masquerading as Python and caused real
  regressions to go unnoticed.
