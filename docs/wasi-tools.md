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
   `sharrattj/dash` `1.0.19`, `wasmer/bash` `1.0.25`, and
   `wasmer/coreutils` `1.0.19`; validates their raw wasm atoms with
   `wasm-tools`.
5. Builds `quickjs-ng/quickjs` `v0.14.0` from source with Zig's
   `wasm32-wasi` C toolchain, `-O3`, and `-msimd128`; the script fails if
   the resulting `qjs.wasm` has no wasm SIMD instructions.
6. Builds the helios `curl-wasi` program from source with the optimized
   release profile into `artifacts/wasi-tools/`.
7. Builds the Helios WASIX conformance WAT modules for thread/futex and
   stack continuation execution into `artifacts/wasix/`.

Artifacts produced:

- `artifacts/python3-root/python3.wasm` — real CPython 3.14 component.
- `artifacts/python3-root/lib/python3.14/` — CPython standard library.
- `artifacts/wasix/dash/dash.wasm` — standard WASIX dash raw module.
- `artifacts/wasix/bash/bash.wasm` — standard Wasmer WASIX Bash raw module.
- `artifacts/wasix/quickjs/qjs.wasm` — QuickJS-NG WASI raw module built
  from source with wasm SIMD enabled.
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

The shell and coreutils artifacts must be official Wasmer package payloads.
QuickJS is built locally from the matching QuickJS-NG source archive so the
Helios wasm artifact uses SIMD instead of forcing Linux down to scalar code.
Because the artifact contains wasm SIMD instructions, `boot-artifacts.toml`
marks it with `requires_wasm_simd = true`; kernel prebuilds exclude QuickJS on
targets without wasm SIMD support (riscv64gc has no V extension), and
selecting it explicitly there fails fast with a target-mismatch error.
The Fedora/QEMU native QuickJS benchmark inspects
`artifacts/wasix/quickjs/qjs.wasm` before provisioning: if that wasm does not
contain SIMD instructions, the benchmark fails and asks for a rebuilt QuickJS
artifact. When QuickJS is present, native QuickJS is built from the same
QuickJS-NG source with native SIMD enabled.
In an offline environment, pass the raw modules:

```bash
WASIX_DASH_WASM=/path/to/official/dash.wasm \
WASIX_BASH_WASM=/path/to/official/bash.wasm \
QUICKJS_WASM=/path/to/official/qjs.wasm \
COREUTILS_WASM=/path/to/official/coreutils.wasm \
tools/wasi-apps/build.sh
```

`QUICKJS_WASM` must already contain wasm SIMD instructions. To avoid a network
fetch while still building QuickJS locally, pass the source archive:

```bash
QUICKJS_SOURCE_ARCHIVE=/path/to/quickjs-ng-0.14.0.tar.gz \
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
  --boot-program http-client \
  --no-compiler-plugin \
  shell -c '/bin/curl http://neverssl.com/'
```

Expected output contains:

- `NeverSSL`

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
pipeline/redirection/script/cwd/env/exit-status behavior, QuickJS,
CPython stdlib imports, and an ICMP echo against the slirp gateway. Set
`HELIOS_WASI_SMOKE_RELEASE=1` when the smoke should rebuild and boot
release artifacts. Set `HELIOS_WASI_SMOKE_CURL=1` to include the networked
curl smoke and a ping resolved through DNS.

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
CPython, QuickJS, and local TCP throughput. Logs are written under
`target/perf-baselines/`. Set `HELIOS_WORKLOAD_BENCH_WORKLOADS` to a
comma-separated workload list when narrowing a run.

The Helios/Linux gap report has three timing lines: Helios, native Fedora
under QEMU, and Wasmtime running the same wasm artifact inside that Fedora
guest. Wasmtime-on-Linux is the floor: every workload with a
`wasmtime_profile` entry in `tools/wasi-apps/workloads.json` must be beaten by
Helios before the result is acceptable.

```bash
tools/wasi-apps/linux-gap-bench.sh --workload quickjs-loop
```

Both Linux lanes run in one Fedora Cloud guest whose architecture matches the
host: aarch64 on an Apple Silicon or arm64 Linux machine, x86_64 on an x86
machine. A guest of the other architecture only runs under cross-architecture
TCG, where the cloud image's own systemd device and service timeouts expire
before it finishes booting, so the tool refuses that combination and asks for
the two lanes to be split with `--skip-helios` / `--skip-linux` — which is
also how CI compares a Helios lane and a Linux lane running on identical
hardware. `--linux-guest-arch` forces the guest architecture when a host
really can emulate the other one. The accelerator follows the host as well:
HVF on macOS, KVM where `/dev/kvm` is available, otherwise TCG, and a TCG
guest additionally receives generous systemd unit, device, and udev event
timeouts through cloud-init because every guest instruction is translated.

Fedora 44's systemd occasionally fails its manager startup under TCG
("Failed to fork off sandboxing environment for executing generators")
and freezes before sshd comes up. The harness treats this upstream guest
flake as retryable: a boot that does not reach SSH within its budget is
killed, its serial log is preserved as `serial.boot-attempt-N.log`, the
overlay disk is recreated, and the first boot is retried up to three
times before the run fails.

Host-side assets are pinned: the per-architecture Fedora Cloud image is
verified against the SHA256 published in the compose's signed `CHECKSUM` file
on every run, and the Wasmtime Linux release matching the guest architecture
is downloaded and unpacked beside it. Both are cached in the VM asset
directory, and the guest itself still needs no internet access for the
measured run. `--fedora-image-url` requires a matching
`--fedora-image-sha256`; `--wasmtime-linux-bin` and `--wasmtime-linux-archive`
override the staged Wasmtime with a local executable or tar archive for the
guest architecture. Workloads without `wasmtime_profile` are reported as
uncovered by the floor rather than silently approximated through a different
program or ABI. `wasi-tcp-throughput` is the standard WASI sockets network
floor workload: Helios, Fedora native, and Wasmtime-on-Linux all receive the
same deterministic 64 MiB local TCP stream, and Helios and Wasmtime execute
the same wasm component.

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
  --boot-program http-client \
  --no-compiler-plugin \
  shell -c '/bin/curl http://neverssl.com/'
```

Expected output contains:

- `NeverSSL`

## Implementation Notes

- `artifacts/python3-root/python3.wasm` is the real CPython interpreter
  converted from WASI preview1 to the preview2 component we run. In bootfs
  the stdlib is mounted under `usr/local/lib/python3.14/`, matching the
  upstream CPython build prefix.
- `tools/wasi-apps/curl` uses `helios_api::http` (`wasi:http/client`)
  instead of host libc sockets, so it runs correctly inside helios. The
  kernel forwards each request to the `http-client` kernel plugin
  (`programs/http-client`), which speaks HTTP/1.1 over `helios:system/net`;
  the plugin must be provisioned (`--boot-program http-client`) or
  `wasi:http/client` reports `configuration-error`. `https://` returns the
  typed `TLS-protocol-error` until the TLS transport lands.
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
