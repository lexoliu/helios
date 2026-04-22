# WASI Python/Curl On Helios

This document captures the exact workflow to stage and run `python` and
`curl` as WASI programs on macOS, then execute them inside Helios through
a shared filesystem.

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
4. Builds the helios `curl-wasi` program from source into
   `artifacts/wasi-tools/`.

Artifacts produced:

- `artifacts/python3-root/python3.wasm` — real CPython 3.14 component
  (~30 MB).
- `artifacts/python3-root/lib/python3.14/` — CPython standard library.
- `artifacts/wasi-tools/curl.wasm`
- `artifacts/wasi-tools/curl-stripped.wasm`

The CPython download requires network. To re-stage in an offline
environment, place a pre-downloaded
`python-${VERSION}-wasi_sdk-24.zip` somewhere and pass its path via
`CPYTHON_WASI_ZIP=/path/to/zip tools/wasi-apps/build.sh`.

## Run In Helios (RISC-V VM)

Use inspector `vm` mode with repository root as the shared directory.
Use `--release` when the VM command should rebuild the guest kernel plus
embedded user-space programs (`init`, `debugger`) in release mode before
boot. CPython needs substantial guest memory; pass `--memory 2G`.

```bash
cargo run -p helios-inspector -- vm --arch riscv64 --memory 2G \
  --shared-dir "$PWD" \
  shell -c '/host/artifacts/python3-root/python3.wasm -c "print(40+2)"'
```

Expected output includes:

```text
42
```

```bash
cargo run -p helios-inspector -- vm --arch riscv64 --memory 2G \
  --shared-dir "$PWD" \
  shell -c '/host/artifacts/wasi-tools/curl-stripped.wasm http://example.com'
```

Expected output contains:

- `<!doctype html>`
- `<title>Example Domain</title>`

## Implementation Notes

- `artifacts/python3-root/python3.wasm` is the real CPython interpreter
  converted from WASI preview1 to the preview2 component we run. It
  derives its install prefix from `argv[0]`, which is why the stdlib
  lives next to it under `lib/python3.14/`.
- `tools/wasi-apps/curl` uses `helios-api::net::TcpStream` (WIT syscall
  surface) instead of host libc sockets, so it runs correctly inside
  helios. It currently supports `http://` URLs only.

## Contract For Future Agents

- Do not remove `tools/wasi-apps/build.sh` or rename output paths
  (`artifacts/python3-root/…`, `artifacts/wasi-tools/…`) without
  updating this document and `README.md`.
- Keep runtime behavior verified with inspector `vm --shared-dir`
  commands above.
- Do not reintroduce the old `tools/wasi-apps/python` stub — it was a
  Rust-level evalexpr toy masquerading as Python and caused real
  regressions to go unnoticed.
