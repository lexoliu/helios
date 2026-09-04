# Wasmtime Dependency

Helios uses the sibling Wasmtime checkout through the workspace path dependency
at `../wasmtime/crates/wasmtime`.

## Required revision

The local checkout must be at (branch `helios/fiber-block-on-current`):

```text
f9ea747c52b65aaa1b9216d6ac6d2a6c207e345a
```

CI does not clone that history. It checks out the vendored snapshot of the same
tree, `lexoliu/wasmtime@6bbaceda21b3de992508f1c26e45f66bfd175e68` (branch
`helios-vendored`), whose `crates/`, `cranelift/`, `winch/` and `pulley/`
directories are identical to the revision above. The snapshot pin lives in one
place, `.github/actions/checkout-wasmtime/action.yml`, and every workflow that
needs the workspace to resolve uses that action. Update the pin there and the
revision here in the same change.

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

## Per-target memory profile

Every target helios runs on — `aarch64-unknown-none`, `riscv64gc-unknown-none-elf`,
`x86_64-unknown-none` and `hosted/` — uses one memory profile, and there is no
second one to fall back to. The engine asserts the backend provides
lazy-commit virtual memory (`Cpu::has_lazy_commit_virtual_memory`) and then
configures the pooling instance allocator with:

| Setting | Value | Why |
| --- | --- | --- |
| `memory_reservation` | 4 GiB (`helios_artifact::CWASM_MEMORY_RESERVATION`) | A wasm32 guest cannot form an address outside it, so Cranelift emits no bounds check. |
| `memory_guard_size` | 32 MiB (`helios_artifact::CWASM_MEMORY_GUARD_SIZE`) | A static offset smaller than this is folded into the access instead of being checked. |
| `memory_init_cow` | `true` | Instance initialization maps the module image rather than copying it. |
| `memory_may_move` | `false` | The reservation the compiled code assumes never relocates. |
| `signals_based_traps` | `true` | The fault a reserved page raises is what the runtime turns into a wasm trap. |

The same two constants are what the compiler plugin compiles cwasm artifacts
against (`compiler-support`), because a module's elided bounds checks are only
sound against the reservation it was compiled for. Both halves read
`helios-artifact`; `engine_resolves_the_lazy_commit_memory_profile` in
`kernel/src/wasmtime_adapter/engine.rs` asserts the built engine still resolves
to them.

Each backend serves that profile from its own `hal::vmm::AddressSpace` through
the `custom-virtual-memory` C ABI in `kernel/src/wasmtime_adapter/custom_vm.rs`,
out of a user-VA window large enough for the reservations:

| Backend | Paging | User-VA window |
| --- | --- | --- |
| aarch64 | 4-level, `TTBR1` | `0xFFFF_C000_0000_0000..0xFFFF_E000_0000_0000` (32 TiB) |
| riscv64 | Sv48 | `0x0000_2000_0000_0000..0x0000_4000_0000_0000` (32 TiB) |
| x86_64 | 4-level, Limine `CR3` | `0x0000_2000_0000_0000..0x0000_4000_0000_0000` (32 TiB) |

The pool reserves `memory_reservation + memory_guard_size` per slot across
`total_memories` slots — about 3.9 TiB per engine, and the kernel builds one
engine for system components and one for launched programs. That is why riscv64
runs Sv48 rather than Sv39: Sv39's entire 512 GiB address space cannot hold one
engine's worth, and the backend fails at `activate_paging` if a hart refuses
Sv48 rather than falling back to a profile the compiled code does not match.

Swap (#25) rides on the same reservations but needs a second piece from each
backend: a not-present page-table encoding that carries a swap token, installed
as a `SwapVmHooks` table. Only aarch64 has that today; riscv64 and x86_64 call
`disable_swap(SwapDisabled::NoSwapHooks)` until they do.

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
