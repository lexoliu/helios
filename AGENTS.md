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
  timer, component host, observer, capability resources, host-fs client, and
  every WIT service implementation. It is `#![no_std]` and generic over `Cpu`.
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
  executable-code publication and ISA feature probing).
- Runtime consumers in `kernel/` depend on traits (for example
  `ComponentRuntimeFactory`, `HostFileSystem`), not on concrete Wasmtime types.
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
- Prefer `use` imports for repeated module, enum, and type paths. Do not keep
  spelling long fully-qualified paths inline when a local import makes the
  code clearer.
- Use `thiserror` for error types. Use `tracing` for diagnostics; never
  `println!`.
- Never use `#[path = "…"]` to pull source files across crate boundaries.

## 3.1 Kernel plugins

- `Kernel plugin` is the authoritative term for an ordinary user-mode wasm
  program running inside Wasmtime with the normal runtime isolation model.
- Kernel plugins are not a separate execution class. The distinction is in
  provisioning and lifecycle management, not in a different runtime boundary.
- Non-core kernel functionality may depend on kernel plugins.
- The compiler is a kernel plugin: it is bootfs-provisioned, loaded during
  kernel startup, and trusted by the kernel for signed `cwasm` output.

## 3.2 Naming

- Directory names must not use the `helios` prefix. Use concise names such as
  `cli/` rather than `helios-cli/`.
- Package and crate names may still use the `helios-` prefix when that matches
  workspace naming conventions.

## 4. Async-first execution

The kernel runs a cooperative async executor; anything that pins it blocks
every other task, including the 9p host-fs transport, WASI futures, and
timers. The rules below apply to every crate that is either `#![no_std]` or
driven by the cooperative executor (hal, kernel, riscv, x86, hosted runtime
paths, components).

- **Do not call `block_on` in production code.** The only legitimate uses
  are (a) tests, (b) bootstrap entry points that run *before* the kernel
  executor starts (e.g. `run_system_component`), and (c) the `block_on`
  definition itself. If you feel you need `block_on` elsewhere, make the
  caller async instead.
- **Never make an async operation look synchronous.** If an operation
  reaches a trait that implements it with `.await`, the public entry point
  must also be `async` (or return an `impl Future`). Do not hide an async
  call behind a sync shim by `block_on`-ing internally.
- **Never busy-wait on external state inside an async context.**
  `core::hint::spin_loop()` is only acceptable for sub-microsecond hardware
  synchronisation (UART TX FIFO drain, virtio descriptor completion,
  critical-section contention). For anything that waits on software state
  (I/O readiness, channel message, transport response), use `Notify`,
  oneshot channels, or `yield_now().await`.
- **Non-blocking adapters for blocking APIs.** Serial readers, channel
  receivers, and similar interfaces expose a non-blocking "try" variant
  (`try_read_serial`, `try_recv`, etc.) so async callers can yield between
  polls. Blocking variants exist only for bootstrap paths.
- **No `Arc<Mutex<T>>` hidden behind async APIs.** Prefer channels, kernel
  `Notify`, or single-owner `RefCell` within a task. When a lock is
  unavoidable, use the kernel's async `Mutex`/`RwLock` from
  `kernel/src/sync.rs` and release the guard before `.await` points.
- **Yielding, not spinning, is the currency.** Any loop that polls for
  progress must `yield_now().await` on the non-ready path, giving the
  executor a chance to drive the task that produces the progress.

## 5. Inspector surface

- Inspector commands are exactly `shell`, `stats`, `tracing`, `repl`, and
  `vm`. Do not re-introduce `qemu-shell` or any legacy `dash` CLI entry.
- `repl` treats `stats` and `tracing` as local shortcuts and forwards all
  other input to the guest shell.
- `vm` boots the selected architecture under QEMU, waits for the guest
  debugger component to come up, then drops into a `repl` session.
- Inspector ↔ guest communication must go through WIT RPC defined in
  `helios-inspector-protocol`, not through ad-hoc side channels.

## 6. WASI tooling

- `tools/wasi-apps/build.sh` is the single entry point that stages WASI
  shared-fs tooling. It downloads the official CPython WASI build and
  places it under `artifacts/python3-root/` (python3.wasm + stdlib),
  and builds our Rust `curl-wasi` under `artifacts/wasi-tools/`.
- Do not reintroduce a hand-rolled Python interpreter (the old
  `tools/wasi-apps/python` stub). helios tests against real CPython or
  nothing.
- `docs/wasi-tools.md` documents the reproducible workflow. If runtime
  behaviour changes, update docs and paths atomically in the same change.

## 7. Required checks before finishing a task

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
