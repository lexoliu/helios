# Wasmtime Dependency

Helios uses the sibling Wasmtime checkout through the workspace path dependency
at `../wasmtime/crates/wasmtime`.

## Required revision

The checkout must be at:

```text
11218b3b9269f1fb50df1f468980708c6fc3ce4e
```

This revision is based on upstream Wasmtime commit:

```text
2fb59986ce75a005e0d12fd4637f2e772bb659fd
```

It corresponds to Wasmtime 48.0.0-dev and requires Rust 1.95 or newer. Helios
pins `nightly-2026-06-15` in `rust-toolchain.toml` for Rust 1.98 and installs all
host, bare-metal, and WASI targets used by the workspace.

## Patch series

The local `helios-upstream-series-ready` branch is organized as a stacked,
independently reviewable series. Each `pr-ready/*` branch points at the tip intended
for that upstream pull request:

1. `pr-ready/no-std-threads` — host-provided synchronization primitives and no-std
   WebAssembly threads support.
2. `pr-ready/no-std-pooling` — no-std pooling allocator, including deterministic
   single-shard selection where host thread-local storage is unavailable.
3. `pr-ready/generic-fiber-pool` — bounded reuse of generic async fiber stacks.
4. `pr-ready/custom-vm-accessible-reset` — reset only the accessible prefix of
   custom-VM linear memories.
5. `pr-ready/global-pass-timing` — aggregate Cranelift pass timings published by
   parallel compiler workers.
6. `pr-ready/riscv64-simd-fail-fast` — reject SIMD and relaxed-SIMD during
   configuration validation when a riscv64 target does not enable the V
   extension.
7. `pr-ready/riscv64-inline-copy` — scalarize fixed-size inline copies when an ISA
   lacks native 128-bit SIMD. This fixes GC `array.copy` compilation on
   riscv64 without the V extension and includes a target-specific disassembly
   regression test.

The final commit on `helios-upstream-series-ready` remains a Helios integration
patch: it separates Component Model Async ABI compilation
from Wasmtime's async runtime/fiber support so the compiler plugin can produce
compatible artifacts while the no-std kernel only loads and runs precompiled
code. It should remain in the fork unless upstream agrees on that feature
boundary.

Two former fork patches were removed because upstream already provides their
functionality:

- Cranelift pass timing already computes `now.duration_since(start)` correctly.
- Component async TLS already uses slot 1 of the custom-platform
  `wasmtime_tls_get`/`wasmtime_tls_set` ABI. Helios implements both Wasmtime TLS
  slots through `WasmtimeTlsSlots`; the removed standalone component-TLS symbols
  must not be reintroduced.

Do not submit replacement patches for those upstream implementations.

Helios Preview 3 linker bindings are generated from the repository's own
`wit/` contract, not from Wasmtime's sibling `crates/wasi/src/p3/wit` tree.
This is intentional: the repository's `wit/` tree is the single source of
truth shared by the kernel linker and by every guest (`helios-api` generates
from the same directory), so the two halves can never drift into
incompatible package identities. The files currently track the released
`0.3.0` packages plus `wasi:http@0.3.0`; refresh them from Wasmtime's
`crates/wasi/src/p3/wit/deps` and `crates/wasi-http/src/p3/wit/deps` when the
vendored revision moves, rather than pointing bindgen at the sibling tree.

## Runtime-performance context

The generic pooling allocator keeps a bounded set of warm async fiber stacks on
non-Unix targets. The custom-VM reset change limits anonymous-memory reset to
the currently accessible linear-memory prefix. On AArch64/HVF, the original
`quickjs-loop` profile showed one 8 MiB async stack allocation per run before
stack reuse; limiting custom-VM reset then moved the profiled median from 57 ms
to 46 ms by avoiding a full static-reservation page-table scan on Store drop.

After changing the required revision, run the full check matrix from the
repository root and recapture the canonical release AOT baseline described in
`CLAUDE.md` §3.5.
