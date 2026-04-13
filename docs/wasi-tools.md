# WASI Python/Curl On Helios

This document captures the exact workflow to build and run `python` and `curl` as WASI programs on macOS, then execute them inside Helios through a shared filesystem.

## Build Artifacts

From repository root:

```bash
tools/wasi-apps/build.sh
```

Artifacts are produced at:

- `artifacts/wasi-tools/python.wasm`
- `artifacts/wasi-tools/python-stripped.wasm`
- `artifacts/wasi-tools/curl.wasm`
- `artifacts/wasi-tools/curl-stripped.wasm`

## Run In Helios (RISC-V VM)

Use inspector `vm` mode with repository root as the shared directory.
Use `--release` when the VM command should rebuild the guest kernel plus embedded user-space programs such as `init` and `debugger` in release mode before boot.

```bash
cargo run -p helios-inspector -- vm --arch riscv64 --no-build \
  --shared-dir "$PWD" \
  shell -c 'exec /host/artifacts/wasi-tools/python-stripped.wasm -c "print(40+2)"'
```

Expected output:

```text
42
```

```bash
cargo run -p helios-inspector -- vm --arch riscv64 --no-build \
  --shared-dir "$PWD" \
  shell -c 'exec /host/artifacts/wasi-tools/curl-stripped.wasm http://example.com'
```

Expected output contains:

- `<!doctype html>`
- `<title>Example Domain</title>`

## Implementation Notes

- `tools/wasi-apps/python` is an expression-oriented Python subset for Helios userland automation.
  - Current supported statement form: `print(<expr>)`.
  - Supports `-c` and script-file modes.
- `tools/wasi-apps/curl` uses `helios-api::net::TcpStream` (WIT syscall surface) instead of host libc sockets, so it runs correctly inside Helios.
- `curl` currently supports `http://` URLs.

## Contract For Future Agents

- Do not remove `tools/wasi-apps/build.sh` or change output file names without updating this document and `README.md`.
- Keep runtime behavior verified with inspector `vm --shared-dir` commands above.
- If replacing the Python runtime with full CPython/RustPython later, preserve the same CLI entry (`python -c ...`) and output paths in `artifacts/wasi-tools`.
