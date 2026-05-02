# WASI/WASIX Python, Curl, Shells, And QuickJS On Helios

This document captures the exact workflow to stage and run `python`,
`curl`, the shell artifacts, QuickJS, and the shared coreutils binary.
`dash`, `bash`, CPython, QuickJS, coreutils commands, and `curl` are
boot filesystem artifacts recorded in
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
3. Installs `python3.wasm`, checksum/provenance sidecars, and
   `lib/python3.14/` (the CPython stdlib) under
   `artifacts/python3-root/`.
4. Downloads and extracts the pinned official Wasmer WEBc images for
   `sharrattj/dash` `1.0.19`, `wasmer/bash` `1.0.25`,
   `saghul/quickjs` `0.0.3`, and `wasmer/coreutils` `1.0.19`;
   validates their raw wasm atoms with `wasm-tools`; and records
   checksum plus `SOURCE.txt` provenance files under `artifacts/wasix/`.
5. Builds the helios `curl-wasi` program from source into
   `artifacts/wasi-tools/`.

Artifacts produced:

- `artifacts/python3-root/python3.wasm` — real CPython 3.14 component.
- `artifacts/python3-root/python3.wasm.sha256` — checksum consumed by
  `helios-cli kernel-prebuild`.
- `artifacts/python3-root/SOURCE.txt` — source package, version, URL, and
  checksum record.
- `artifacts/python3-root/lib/python3.14/` — CPython standard library.
- `artifacts/wasix/dash/dash.wasm` — standard WASIX dash raw module.
- `artifacts/wasix/dash/dash.wasm.sha256` — checksum consumed by
  `helios-cli kernel-prebuild`.
- `artifacts/wasix/dash/SOURCE.txt` — source package, version, URL, and
  checksum record.
- `artifacts/wasix/bash/bash.wasm` — standard Wasmer WASIX Bash raw module.
- `artifacts/wasix/bash/bash.wasm.sha256`
- `artifacts/wasix/bash/SOURCE.txt`
- `artifacts/wasix/quickjs/qjs.wasm` — standard QuickJS WASI raw module.
- `artifacts/wasix/quickjs/qjs.wasm.sha256`
- `artifacts/wasix/quickjs/SOURCE.txt`
- `artifacts/wasix/coreutils/coreutils.wasm` — standard Wasmer
  coreutils WASIX raw module. `boot-artifacts.toml` exposes the same
  module as `/bin/cat`, `/bin/env`, `/bin/head`, `/bin/ls`,
  `/bin/mkdir`, and `/bin/pwd`.
- `artifacts/wasix/coreutils/coreutils.wasm.sha256`
- `artifacts/wasix/coreutils/SOURCE.txt`
- `artifacts/wasi-tools/curl.wasm`
- `artifacts/wasi-tools/SOURCE.txt`
- `artifacts/wasi-tools/curl-stripped.wasm`

The CPython download requires network. To re-stage in an offline
environment, place a pre-downloaded
`python-${VERSION}-wasi_sdk-24.zip` somewhere and pass its path via
`CPYTHON_WASI_ZIP=/path/to/zip tools/wasi-apps/build.sh`.

The shell and QuickJS artifacts must be official Wasmer package payloads,
not repo-local stubs. In an offline environment, either pass the raw
modules:

```bash
WASIX_DASH_WASM=/path/to/official/dash.wasm \
WASIX_BASH_WASM=/path/to/official/bash.wasm \
QUICKJS_WASM=/path/to/official/qjs.wasm \
COREUTILS_WASM=/path/to/official/coreutils.wasm \
tools/wasi-apps/build.sh
```

or pass the pinned WEBc images:

```bash
WASIX_DASH_WEBC=/path/to/dash.webc \
WASIX_BASH_WEBC=/path/to/bash.webc \
QUICKJS_WEBC=/path/to/quickjs.webc \
COREUTILS_WEBC=/path/to/coreutils.webc \
tools/wasi-apps/build.sh
```

`helios-cli kernel-prebuild` verifies the checksum sidecar before adding
the artifact to bootfs and checks every external artifact's `SOURCE.txt`
against `tools/wasi-apps/boot-artifacts.toml`. A missing or mismatched
artifact is a hard error; there is no repo-local fallback.

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

The same path runs the standard Bash artifact:

```bash
cargo run -p helios-inspector -- vm --arch aarch64 --release \
  --boot-program dash --boot-program debugger --boot-program bash \
  --no-compiler-plugin \
  shell -c '/bin/bash -c "echo bash:$((20+22))"'
```

Expected output includes:

```text
bash:42
```

QuickJS can execute JavaScript from the boot filesystem:

```bash
cargo run -p helios-inspector -- vm --arch aarch64 --release \
  --boot-program dash --boot-program debugger --boot-program quickjs \
  --no-compiler-plugin \
  shell -c '/bin/qjs -e "console.log(40+2)"'
```

Expected output includes:

```text
42
```

The staged coreutils module provides ordinary shell helpers used by the
WASIX shell smoke tests:

```bash
cargo run -p helios-inspector -- vm --arch aarch64 --release \
  --boot-program dash --boot-program debugger --boot-program bash \
  --boot-program mkdir --boot-program cat \
  --no-compiler-plugin \
  shell -c '/bin/bash -c "cd /; /bin/mkdir d; echo ok > /d/f; /bin/cat /d/f"'
```

Expected output includes:

```text
ok
```

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
- `tools/wasi-apps/extract-webc-wasm.pl` extracts the single raw wasm atom
  from the pinned Wasmer WEBc images. The build script validates both WEBc
  and raw wasm SHA-256 digests before staging.
- `wasmer/coreutils` is a multi-call WASIX module. Helios records each
  exposed command as its own bootfs path while preserving one source
  artifact and one provenance record.

## Contract For Future Agents

- Do not remove `tools/wasi-apps/build.sh` or rename output paths
  (`artifacts/python3-root/…`, `artifacts/wasix/dash/…`,
  `artifacts/wasi-tools/…`) without updating this document and
  `README.md`.
- Keep runtime behavior verified with the inspector `vm` commands above.
- Do not reintroduce repo-local shell, coreutils, or language stubs. The
  boot shells, coreutils, and QuickJS are standard Wasmer artifacts
  declared in
  `tools/wasi-apps/boot-artifacts.toml`.
- Do not reintroduce the old `tools/wasi-apps/python` stub — it was a
  Rust-level evalexpr toy masquerading as Python and caused real
  regressions to go unnoticed.
