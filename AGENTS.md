# Agent Contract

This file describes the architectural rules that any contributor — human or
AI agent — must respect when changing this repository. These rules encode
decisions that have already been made; they are not up for renegotiation on a
per-task basis.

## 0. Precedence over global agent rules

These project rules override any conflicting rule from a global agent
configuration (for example `~/.claude/CLAUDE.md`). In particular: §3.2
requires `--release` artifacts as the only valid evidence for any performance
test, benchmark, or runtime acceptance measurement. Global "do not use
`--release`" guidance does not apply to this repository's performance work.

The Wasmtime path dependency at `../wasmtime/...` is intentional. It is
expected to track the upstream commit that the kernel target builds against;
treat it as a vendored sibling checkout rather than a transient experiment.
When updating the dependency, update its commit hash in `docs/wasmtime.md`
and verify that every required check in §7 still passes against the new
commit.

## 1. Layering

Helios is organised as a strict, one-way dependency stack:

```
hal  <-  kernel  <-  {riscv, x86, hosted}  <-  inspector / programs
```

- **`hal/`** contains architecture-neutral hardware contracts only: traits,
  capability types, and value types. It must not contain concrete drivers or
  runtime logic. `hal/` must also not name any specific runtime or higher-
  layer consumer in its API surface — Wasmtime, Cranelift, the kernel's
  WIT interfaces, the inspector protocol, etc. are all upward-layer concepts
  that leak abstractions if they appear in `hal/`. Capabilities express the
  underlying *hardware* property (e.g. "supports lazy-commit virtual
  memory") and the kernel translates that into runtime knobs (e.g.
  enabling Wasmtime's pooling instance allocator). A trait method named
  after its caller is a code smell that warrants renaming or relocating to
  `kernel/`.
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
- **`hosted/`** exists for development and test coverage only. Do not use it
  as a performance baseline, optimization profile, or evidence that a kernel
  optimization is valid for real targets. Performance-sensitive runtime
  decisions must be grounded in `riscv/`, `x86/`, or explicit target
  capabilities.

If a piece of logic could reasonably be shared between two backends, it
belongs in `kernel/` (or `hal/` for contracts), not duplicated in the backend
crates.

## 2. Construction over conditional compilation

- The kernel must not select runtimes, probe features, or switch behaviour
  based on `#[cfg(target_arch)]`. Capabilities are injected through the backend
  `Cpu` implementation, including executable-code publication and ISA feature
  probing.
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
- Do not create a new crate just to organize local implementation detail. A
  crate must represent a real shared boundary used by multiple owners or an
  externally meaningful contract. Otherwise place the code in the owning layer
  as a module, usually `kernel/` for hardware-independent runtime logic or
  `hal/` for cross-backend contracts.
- Do not leave legacy code for backwards compatibility. When a feature is
  removed, remove every related path in the same change.
- Do not make unilateral architecture changes while debugging. If a failure
  appears to involve a core subsystem, diagnose that subsystem directly instead
  of replacing it with a different architecture.
- Do not mask runtime, allocator, or scheduler bugs by inflating memory sizes,
  widening budgets, disabling limits, lowering optimization, or adding
  component-specific allocation tricks. If a program, component, or kernel
  plugin unexpectedly exhausts memory, diagnose the real ownership boundary,
  allocator initialization, grow path, and lifecycle semantics. Fix the shared
  abstraction instead of making the failing artifact unusually large.
- Virtio is the current core I/O path. Do not replace virtio-backed boot,
  block, network, host-share, or debug transport paths with IDE, emulated
  legacy devices, ad-hoc host files, or other fallback I/O unless the user
  explicitly approves that architectural change in the current task.
- Prefer third-party crates over hand-rolled implementations when the crate
  is well maintained and no-std friendly.
- Prefer generic types, `impl Trait`, and the type system over enum-based
  type erasure or `dyn Trait` when the set of implementations is known at
  compile time.
- Async trait definitions must spell out the returned future and bounds
  explicitly, for example
  `fn op(&self) -> impl Future<Output = Result<T, E>> + Send + '_`; do not use
  `async fn` in trait definitions. Trait implementations may use direct
  `async fn` implementations when the compiler accepts them.
- Avoid heap allocation in kernel-facing code. Use stack storage, static
  capacity, caller-owned buffers, arenas, or explicit ownership passed through
  typed APIs whenever the size/lifetime is known. Allocating containers such as
  `Vec`, `String`, `Box`, `Arc`, and map types require a concrete reason tied
  to variable-sized guest data, kernel plugin payloads, or runtime ownership.
- Kernel memory and user memory are separate ownership domains. The kernel
  itself should be as close to zero-allocation as practical and allocate only
  from kernel-owned pools when allocation is unavoidable. User-mode wasm
  instances, including kernel plugins, use separate user-memory allocation and
  resource accounting; user OOM is handled by killing/dropping the affected
  wasm instance and reclaiming its user-memory pool, not by growing kernel
  budgets or adding per-plugin memory policy. Kernel OOM is fatal and must
  panic rather than attempting recovery.
- Avoid dynamic dispatch in kernel-facing code. Do not introduce `dyn Trait`,
  `Box<dyn ...>`, `Arc<dyn ...>`, or type-erased callback surfaces when a
  generic type, associated type, or concrete adapter can express the boundary.
- Prefer `use` imports for repeated module, enum, and type paths. Do not keep
  spelling long fully-qualified paths inline when a local import makes the
  code clearer.
- `anyhow` is not allowed in this repository. Use typed error enums with
  `thiserror`; callers may translate those errors at external CLI or test
  boundaries, but repository crates must preserve structured error provenance.
- Diagnostics policy:
  - `tracing` is the only diagnostic crate in this repository. Never use the
    `log` crate, no matter the scope. If a third-party dependency emits `log`
    records (for example `cranelift_codegen`), bridge them with
    `tracing-log::LogTracer` rather than installing a `log::Log` impl.
  - In kernel-side, `hal/`, library, and protocol crates, never use
    `println!`/`eprintln!`/`dbg!`; route diagnostics through `tracing`.
  - In user-mode wasm programs (anything under `programs/`, plus kernel
    plugins per §3.1), `println!` and `print!` are the natural way to write
    to stdio and are explicitly allowed; do not bend them through
    `helios_api::io::stdout()` for the sake of the rule.
- Never use `#[path = "…"]` to pull source files across crate boundaries.

## 3.1 Kernel plugins

- `Kernel plugin` is the authoritative term for an ordinary user-mode wasm
  program running inside Wasmtime with the normal runtime isolation model.
- Kernel plugins are not a separate execution class. The distinction is in
  provisioning and lifecycle management, not in a different runtime boundary.
- Non-core kernel functionality may depend on kernel plugins.
- The compiler is a kernel plugin: it is bootfs-provisioned, loaded during
  kernel startup, and trusted by the kernel for signed `cwasm` output.
- The HTTP client is a kernel plugin (`programs/http-client`): the kernel
  implements only `wasi:http/types` and forwards `wasi:http/client.send`
  through a typed provider slot to the plugin's exported
  `wasi:http/handler`; HTTP/1.1 framing, DNS, and (later) TLS run in user
  memory. The same provider-slot routing is the path for any future
  interface served by a plugin.
- Kernel plugins and ordinary user-mode programs share the same user-memory
  and allocation contracts. Do not add plugin-private allocator policy, custom
  memory floors, or oversized linker memory settings to work around OOM. A
  kernel plugin may fail with a typed OOM result and be discarded/restarted by
  its owner; that lifecycle belongs in the shared runtime contract.

## 3.2 Wasmtime runtime performance

- The kernel must provide an internal exception/signal mechanism usable by
  Wasmtime runtime code. Do not treat `Config::signals_based_traps(false)` as
  a final solution; disabling signals-based traps removes important Wasmtime
  performance paths such as guard-page bounds-check elimination and Winch
  compatibility.
- Wasmtime performance features such as typed function references, SIMD,
  relaxed SIMD, and target ISA feature probing must remain correctly enabled
  when the real target supports them.
- Full x86 AVX/FMA/AVX512 enablement is an explicit TODO until the x86 kernel
  provides OSXSAVE, XCR0 configuration, and XSAVE/XRSTOR state preservation.
- `hosted/` must not be used as evidence for Wasmtime runtime performance
  decisions. Runtime optimization choices must be based on real kernel targets
  and explicit target capabilities.
- Any performance test, benchmark, or acceptance measurement must run with
  optimized `--release` artifacts. Debug builds are acceptable for functional
  checks and diagnosis only; do not use them as performance evidence.
- Every Wasmtime perf opt-in the kernel can support must stay enabled:
  pooling instance allocation, signals-based traps, epoch interruption,
  SIMD/threads/component-model, `async_stack_zeroing(false)`. Once the user
  VM is real, `memory_reservation` and `memory_guard_size` must eliminate
  wasm32 bounds checks, with `memory_init_cow(true)` and
  `memory_may_move(false)`. Disabling any of these requires a same-PR
  justification of the alternative semantic path.

## 3.3 Naming

- Directory names must not use the `helios` prefix. Use concise names such as
  `cli/` rather than `helios-cli/`.
- Package and crate names may still use the `helios-` prefix when that matches
  workspace naming conventions.

## 3.4 Modern hardware and SMP-first

Helios targets modern multi-core hardware. SMP correctness is a day-one
property of every kernel and `hal/` subsystem, never a follow-up.

- New abstractions state their concurrency contract before their API: which
  ops are lock-free, which take an async mutex, which fan out to remote
  processors. "Add SMP later" is not an acceptable design note.
- Any address-space mutation invalidates the local TLB and dispatches an IPI
  shootdown to every other processor that has run in that space. Skipping
  the IPI is a regression.
- Hot paths use per-CPU storage indexed by `Cpu::current_processor()`, and
  cross-processor queues prefer atomics or lock-free channels over `Mutex`.
- Hardware features that already exist on the target (XSAVE/AVX, MWAIT,
  cache-coherent I/O, gicv3) must be brought up properly rather than
  avoided because they need SMP-aware setup.

## 3.5 Performance baselines

Capture a baseline before any change that affects kernel-side runtime
performance, then compare after. Baseline logs and reports live in
`target/perf-baselines/` and are not committed; cite the median
`elapsed_ms` and any regression directly in the PR description.

The canonical workload is the in-kernel compiler plugin compiling a fixed
wasm input under arm64+HVF (fastest supported surface, no TCG noise):

```bash
./target/release/helios-inspector vm --arch aarch64 --release \
    aot-bench artifacts/wasi-tools/curl.wasm --iterations 5 \
    | tee target/perf-baselines/aarch64-hvf-curl-<short-sha>.log
```

The regression target is the median `elapsed_ms` across iterations
2..N — iteration 1 is the cold cache-build cost and excluded from
steady-state comparisons. `--compiler-timing` adds a cranelift pass
breakdown to the log; use it for diagnosis when a regression appears,
not for the canonical baseline.

Network-path changes take a second baseline, because the compiler
workload never touches the NIC. The canonical network workload is
`tcp-throughput` on a multi-queue backend — `user` (slirp) is a
single-queue, no-offload path and is not valid evidence for anything the
virtio-net driver negotiates:

```bash
helios-inspector vm net-setup \
    --net-backend tap --net-ifname helios0 --net-bridge helios-br0 --net-dhcp
./target/release/helios-inspector vm --arch aarch64 --release \
    --net-backend tap --net-ifname helios0 --net-bridge helios-br0 \
    --net-queues "$(nproc)" \
    workload-bench --workload tcp-throughput --iterations 5 \
    | tee target/perf-baselines/aarch64-tap-tcp-<short-sha>.jsonl
```

Cite the negotiated feature set alongside the median: the `virtio-net
online` boot line records the queue-pair count and the checksum/TSO bits
the run actually had. `docs/networking.md` covers the backends, the
privileged setup, and what each one can exercise.

## 3.6 Architectural ambition

- Performance, scalability, and architectural cleanliness work must
  not be cut short because the change is large. If an improvement
  better tracks modern high-performance practice and yields a more
  elegant abstraction without violating an existing contract, take
  the full path — including cross-crate interface reshapes,
  ownership reorganization across subsystems, and ABI changes across
  backends.
- Do not estimate effort. Phrases like "X hours", "X days", "quick
  win", "low-effort", or any time-cost framing are not valid
  decision inputs. The only decision criteria are: (a) does the
  change resolve a root problem; (b) does the resulting code track
  modern practice and read more cleanly; (c) does it introduce any
  new contract violation. All three pass → land it.
- Do not propose a degraded variant in order to "ship something
  first". If the only options are a degraded variant or no change,
  do not change — record the decision as a real blocker for
  explicit user discussion, and do not freelance an alternative.
- Large changes should be discussed proactively (via AskUserQuestion
  or normal conversation) to align on intent and direction. Such
  discussions are for alignment, not for requesting permission to
  land a degraded variant.

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
- `helios-inspector vm --kernel-debug --gdb <endpoint>` is the supported QEMU
  gdbstub path for symbol-level kernel debugging. Use `--gdb-wait` when the
  debugger must attach before kernel entry. Use endpoints such as `tcp::1234`
  with GDB (`target remote :1234`) or LLDB (`gdb-remote 1234`). Keep this path
  working for both `riscv64` and `x86-64`.
- `helios-inspector vm --debug` is the shortcut for local kernel debugging:
  it enables the kernel debug profile, opens the default gdbstub, waits for
  the debugger before kernel entry, keeps the runtime directory, and exposes
  monitor/QMP sockets. Prefer this over hand-written QEMU invocations.
- Inspector VM must expose practical diagnosis knobs for future work:
  retained runtime directories, QEMU stdout/stderr logs, QEMU `-d` trace logs,
  HMP monitor sockets, QMP sockets, CPU/accelerator overrides, and explicit
  raw QEMU argument passthrough. Add missing shortcuts to inspector instead of
  making developers repeat long manual QEMU commands.
- When a QEMU VM appears stuck or silent, use the inspector/QEMU debug path
  flexibly: inspect the QEMU process, gdbstub, kernel symbols, serial socket,
  and QEMU logs before drawing conclusions. Do not replace real target
  debugging with `hosted/` evidence.
- Boot and kernel debugging must be evidence-first: reproduce the failure,
  capture serial/QEMU logs plus GDB/LLDB/QMP state when needed, and only then
  change boot, device topology, trap, or runtime code.
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

Generate the matching `helios-cli kernel-prebuild` manifest first and pass it
through `HELIOS_KERNEL_PREBUILD_MANIFEST` for the target being checked.

Run these locally before declaring a change complete:

```bash
just check-host
just check-target aarch64-unknown-none helios-aarch64
just check-target riscv64gc-unknown-none-elf helios-riscv
just check-target x86_64-unknown-none helios-x86
just test-embedded-debugger
just lint
just test-units
```

`just lint` is `tools/fmt.sh --check` plus `cargo clippy … -D warnings`
over the host crates, each guest program, and all three bare-metal targets;
`just test-units` runs the `hal`, `virtio`, `netstack` and `kernel` unit tests
and the `hal_layering` layering test. CI runs the same recipes, split across
one lane each, so a red lane names the surface that broke.

Clippy is gated at `-D warnings`, and the only lint the workspace turns off is
`clippy::manual_async_fn`, which contradicts §3's explicit-future trait style;
it is allowed once in the root `Cargo.toml` and inherited by every crate
through `[lints] workspace = true`. Suppressing any other lint needs an
`#[expect(…, reason = "…")]` on the item that says why the lint is wrong
there.

Contract compliance is enforced by module ownership and by these checks, not
by guard scripts.
