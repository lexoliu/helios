# WASI/WASIX Python, Curl, Shells, And QuickJS On Helios

This document captures the exact workflow to stage and run `python`,
`curl`, the shell artifacts, QuickJS, and the shared coreutils binary.
`dash`, `bash`, CPython, QuickJS, coreutils commands, `curl`, and the
Helios WASIX conformance modules are boot filesystem artifacts recorded in
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
3. Installs `python3.wasm` and `lib/python3.14/` (the CPython stdlib)
   under `artifacts/python3-root/`.
4. Downloads and extracts the pinned official Wasmer WEBc images for
   `sharrattj/dash` `1.0.19`, `wasmer/bash` `1.0.25`,
   `quickjs-ng/quickjs` `v0.14.0`, and `wasmer/coreutils` `1.0.19`;
   validates their raw wasm atoms with `wasm-tools`.
5. Builds the helios `curl-wasi` program from source with the optimized
   release profile into `artifacts/wasi-tools/`.
6. Builds the Helios WASIX conformance WAT modules for thread/futex and
   stack continuation execution into `artifacts/wasix/`.

Artifacts produced:

- `artifacts/python3-root/python3.wasm` — real CPython 3.14 component.
- `artifacts/python3-root/lib/python3.14/` — CPython standard library.
- `artifacts/wasix/dash/dash.wasm` — standard WASIX dash raw module.
- `artifacts/wasix/bash/bash.wasm` — standard Wasmer WASIX Bash raw module.
- `artifacts/wasix/quickjs/qjs.wasm` — standard QuickJS WASI raw module.
- `artifacts/wasix/coreutils/coreutils.wasm` — standard Wasmer
  coreutils WASIX raw module. `boot-artifacts.toml` exposes the same
  module as `/bin/cat`, `/bin/env`, `/bin/head`, `/bin/ls`,
  `/bin/mkdir`, and `/bin/pwd`.
- `artifacts/wasix/thread-futex/thread-futex.wasm` — Helios WASIX
  conformance module covering `thread_spawn_v2`, `thread_join`,
  `futex_wait`, `futex_wake`, and `thread_exit`.
- `artifacts/wasix/continuation/continuation.wasm` — Helios WASIX
  conformance module covering `stack_checkpoint`, `stack_restore`, and
  the asyncify unwind/rewind exports expected by the adapter.
- `artifacts/wasi-tools/curl.wasm`
- `artifacts/wasi-tools/curl-stripped.wasm`

The CPython download requires network. To re-stage in an offline
environment, place a pre-downloaded
`python-${VERSION}-wasi_sdk-24.zip` somewhere and pass its path via
`CPYTHON_WASI_ZIP=/path/to/zip tools/wasi-apps/build.sh`.

The shell and coreutils artifacts must be official Wasmer package payloads;
QuickJS is staged from the matching official QuickJS-NG WASI release asset.
The Fedora/QEMU native QuickJS benchmark inspects
`artifacts/wasix/quickjs/qjs.wasm` before provisioning: if that wasm contains
SIMD instructions, native QuickJS is built with native SIMD enabled; otherwise
native QuickJS is built with SIMD disabled so `quickjs-loop` does not compare a
vectorized Linux interpreter against a scalar Helios interpreter. Explicit SIMD
throughput is measured by the separate `wasm-simd-lanes` workload.
In an offline environment, pass the raw modules:

```bash
WASIX_DASH_WASM=/path/to/official/dash.wasm \
WASIX_BASH_WASM=/path/to/official/bash.wasm \
QUICKJS_WASM=/path/to/official/qjs.wasm \
COREUTILS_WASM=/path/to/official/coreutils.wasm \
tools/wasi-apps/build.sh
```

For the Wasmer-provided shell/coreutils artifacts, a pinned WEBc image can
also be supplied:

```bash
WASIX_DASH_WEBC=/path/to/dash.webc \
WASIX_BASH_WEBC=/path/to/bash.webc \
COREUTILS_WEBC=/path/to/coreutils.webc \
tools/wasi-apps/build.sh
```

`helios-cli kernel-prebuild` compiles declared boot artifacts into signed
`cwasm` payloads before adding them to bootfs. A missing artifact is a hard
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

The complete local smoke gate for the staged real artifacts is:

```bash
tools/wasi-apps/smoke.sh
```

It runs the AArch64/HVF VM path for standard dash, Bash plus coreutils
pipeline/redirection/script/cwd/env/exit-status behavior, QuickJS, and
CPython stdlib imports. Set
`HELIOS_WASI_SMOKE_RELEASE=1` when the smoke should rebuild and boot
release artifacts. Set `HELIOS_WASI_SMOKE_CURL=1` to include the networked
curl smoke.

The compiler-plugin performance gate for the staged curl artifact is:

```bash
tools/wasi-apps/aot-bench.sh
```

It runs the release AArch64/HVF `aot-bench` workload for
`artifacts/wasi-tools/curl.wasm`, writes the full log under
`target/perf-baselines/`, and prints the median `elapsed_ms` across
iterations 2..N. Set `HELIOS_AOT_BENCH_COMPILER_TIMING=1` to include the
compiler pass breakdown when diagnosing a regression.

The real-software workload performance gate is:

```bash
tools/wasi-apps/workload-bench.sh
```

It boots one release AArch64/HVF VM and measures repeated guest executions
for process startup, stdio pipe throughput, filesystem-heavy shell work,
CPython import-heavy startup, and QuickJS execution. Logs are written under
`target/perf-baselines/`. Set `HELIOS_WORKLOAD_BENCH_WORKLOADS` to a
comma-separated workload list when narrowing a run.

To collect Wasmtime's native Linux profiling artifacts for the same wasm
workloads, pass `--wasmtime-profile-workload <name>` to
`tools/wasi-apps/linux-gap-bench.sh`. The default mode is `jitdump`, which
runs Wasmtime under Linux `perf`, injects Wasmtime's JIT code-load records
with `perf inject --jit`, and writes `perf.data`, `perf.jit.data`, folded
stacks, and an SVG flamegraph. Use `--wasmtime-profile-mode perfmap` for the
lighter perf-map symbol path or `--wasmtime-profile-mode guest` for
Wasmtime's cross-platform Firefox-profiler JSON.

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
- `tools/wasi-apps/wasix-tests/*.wat` are intentionally small Helios-owned
  conformance modules. They do not replace the real dash/bash/QuickJS/CPython
  smoke tests; they pin adapter execution behavior for WASIX threads,
  futexes, and stack continuations.

## Contract For Future Agents

- Do not remove `tools/wasi-apps/build.sh` or rename output paths
  (`artifacts/python3-root/…`, `artifacts/wasix/dash/…`,
  `artifacts/wasix/thread-futex/…`, `artifacts/wasix/continuation/…`,
  `artifacts/wasi-tools/…`) without updating this document and `README.md`.
- Keep runtime behavior verified with the inspector `vm` commands above.
- Do not reintroduce repo-local shell, coreutils, or language stubs. The
  boot shells, coreutils, and QuickJS are standard Wasmer artifacts
  declared in
  `tools/wasi-apps/boot-artifacts.toml`.
- Do not reintroduce the old `tools/wasi-apps/python` stub — it was a
  Rust-level evalexpr toy masquerading as Python and caused real
  regressions to go unnoticed.
