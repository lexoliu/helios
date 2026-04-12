# Agent Contract

This file describes the architectural rules that any contributor — human or
AI agent — must respect when changing this repository. These rules encode
decisions that have already been made; they are not up for renegotiation on a
per-task basis.

## 1. Layering

Helios is organised as a strict, one-way dependency stack:

```
hal  <-  kernel  <-  {riscv, x86, hosted}  <-  inspector / programs
```

- **`hal/`** contains architecture-neutral hardware contracts only: traits,
  capability types, and value types. It must not contain concrete drivers or
  runtime logic.
- **`kernel/`** contains all hardware-independent runtime logic: the executor,
  timer, compute pool, component host, observer, capability resources,
  host-fs client, and every WIT service implementation. It is `#![no_std]` and
  generic over `Cpu`.
- **`riscv/`** and **`x86/`** are concrete hardware adaptation layers. They
  are allowed to contain boot, trap, IRQ, timer, MMIO, UART, virtio, and SMP
  wiring — nothing else. They must not contain WIT worlds, RPC protocol
  handlers, Wasmtime orchestration, or debugger business logic.
- **`hosted/`** is a first-class backend that runs the same kernel on top of
  the host OS. It has the same restrictions as `riscv/` and `x86/`: adapter
  code only, no business logic.

If a piece of logic could reasonably be shared between two backends, it
belongs in `kernel/` (or `hal/` for contracts), not duplicated in the backend
crates.

## 2. Construction over conditional compilation

- The kernel must not select runtimes, probe features, or switch behaviour
  based on `#[cfg(target_arch)]`. Capabilities are injected through separate
  adapter traits supplied by the backend (for example `CodegenPlatform` for
  JIT code publication and ISA feature probing).
- Runtime consumers in `kernel/` depend on traits (for example
  `ProgramRuntimeBackend`, `HostFileSystem`), not on concrete Wasmtime types.
  Concrete runtimes are adapters that satisfy those traits.
- Global mutable state is not allowed. State is passed explicitly through
  `Kernel<CpuImpl>`, `RuntimeState<…>`, and similar owned structures.
- Define Rust service traits before WIT bindings. Model each system capability
  as a Rust trait first, then expose it through a WASI/WIT interface.

## 3. Code quality

- No workaround, fallback, stub, or "simpler approach" code. If a design does
  not fit, fix the abstraction rather than bypassing it.
- No duplicated code across backends. Move shared logic into `kernel/` or
  `hal/` immediately.
- Do not leave legacy code for backwards compatibility. When a feature is
  removed, remove every related path in the same change.
- Prefer third-party crates over hand-rolled implementations when the crate
  is well maintained and no-std friendly.
- Prefer generic types, `impl Trait`, and the type system over enum-based
  type erasure or `dyn Trait` when the set of implementations is known at
  compile time.
- Use `thiserror` for error types. Use `tracing` for diagnostics; never
  `println!`.
- Never use `#[path = "…"]` to pull source files across crate boundaries.

## 4. Inspector surface

- Inspector commands are exactly `shell`, `stats`, `tracing`, `repl`, and
  `vm`. Do not re-introduce `qemu-shell` or any legacy `dash` CLI entry.
- `repl` treats `stats` and `tracing` as local shortcuts and forwards all
  other input to the guest shell.
- `vm` boots the selected architecture under QEMU, waits for the guest
  debugger component to come up, then drops into a `repl` session.
- Inspector ↔ guest communication must go through WIT RPC defined in
  `helios-inspector-protocol`, not through ad-hoc side channels.

## 5. WASI tooling

- `tools/wasi-apps/python` and `tools/wasi-apps/curl` are the source of truth
  for shared-fs WASI tooling. `tools/wasi-apps/build.sh` owns their build
  outputs under `artifacts/wasi-tools/`.
- `docs/wasi-tools.md` documents the reproducible workflow. If runtime
  behaviour changes, update docs and paths atomically in the same change.

## 6. Required checks before finishing a task

Run these locally before declaring a change complete:

```bash
cargo check -p helios-kernel
cargo check -p helios-hosted
cargo check -p helios-inspector
cargo check -p helios-riscv --target riscv64gc-unknown-none-elf
cargo check -p helios-x86 --target x86_64-unknown-none
cargo test  -p helios-hosted init_program::tests::embedded_debugger_ -- --nocapture
```

Contract compliance is enforced by module ownership and by these checks, not
by guard scripts.
