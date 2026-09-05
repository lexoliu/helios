# Helios contributor contract

This file is the repository's contract for every contributor, human or agent.
It records decisions that have already been made; a task does not reopen them.
`CLAUDE.md` is a symlink to this file. A personal or global agent configuration
may add rules for its own user, but where the two disagree this file wins,
because it is the only one every contributor can see.

This file is edited only by a maintainer or by the agent orchestrating a task,
never by a delegated agent. An agent that finds a rule missing or wrong reports
the gap in its result and leaves the wording to the orchestrator.

## 1. The tree and its layering

Dependencies flow one way, from the bottom of this list to the top. A crate
never depends on anything above it.

| Layer | Crates | Contents |
| --- | --- | --- |
| Hardware contracts | `hal/` | Traits, capability types and value types only. `#![no_std]`. |
| Device and protocol libraries | `netstack/`, `virtio/`, `i6300esb/` | `#![no_std]` protocol engines and drivers. `virtio/` builds on `netstack/`; all three depend on `hal/` and none on the kernel. |
| Shared ABIs | `compiler-abi/`, `artifact/` | `#![no_std]` wire formats shared by the kernel and host tools: the compiler plugin's request and response headers, `cwasm` target flags, the signature trailer. |
| Kernel | `kernel/`, `kernel-macro/` | Every piece of hardware-independent runtime logic: executor, timer, memory and OOM, instance registry, component host, Wasmtime adapter, network service, host-fs client, every WIT service. `#![no_std]`, generic over `Cpu`. `kernel-macro/` embeds wasm and the bootfs at build time. |
| Backends | `aarch64/`, `riscv/`, `x86/`, `hosted/` | Boot, trap, IRQ, timer, MMIO, UART, virtio transport and SMP wiring, and nothing else. `hosted/` runs the same kernel on the host OS under the same restriction. |
| Kernel image | the root crate `helios` (`src/`) | The binary that links whichever backend the target selects, and runs `hosted` on the host. |
| User space | `api/`, `api-macro/`, `programs/*` | The userland SDK and the wasm programs, kernel plugins included: `init`, `debugger`, `http-client`, `date`, `ping`, `perf`, `oob-load`. `compiler-plugin/` is the in-kernel compiler, a kernel plugin built by the host tools. |
| Host tools | `inspector/`, `inspector-protocol/`, `cli/`, `compiler-support/`, `workspace-root/` | `std` crates that boot, build and observe a guest. `inspector-protocol/` is the WIT RPC contract between the inspector and the guest debugger. |

The rules that follow from the table:

- `hal/` names hardware properties, never a consumer. Wasmtime, Cranelift, the
  kernel's WIT interfaces and the inspector protocol are all upward-layer
  concepts; a `hal/` trait method named after its caller belongs in `kernel/`.
  A capability says what the hardware can do ("lazy-commit virtual memory")
  and the kernel turns that into a runtime knob (Wasmtime's pooling
  allocator).
- A backend contains adapter code only. WIT worlds, RPC handlers, Wasmtime
  orchestration, debugger logic and anything two backends would both need
  live in `kernel/`, or in `hal/` when it is a contract. Duplicating logic
  across backends is a defect to fix in the same change that notices it.
- `hosted/` exists for development and test coverage. It is never evidence
  for a performance decision, an optimization profile, or the validity of a
  kernel optimization on real targets; those are grounded in the bare-metal
  backends and explicit target capabilities.
- A new crate marks a real boundary used by more than one owner or an
  externally meaningful contract. Local implementation detail is a module in
  the owning layer.
- Directory names carry no `helios` prefix (`cli/`, not `helios-cli/`); package
  names may (`helios-cli`) where the workspace convention wants them.

### Wasmtime

The kernel builds against the sibling checkout at `../wasmtime/crates/wasmtime`
through a workspace path dependency. That checkout is a vendored fork, not a
transient experiment: `docs/wasmtime.md` records the branch and the revision
it must be at, and CI checks out the same snapshot. Moving the dependency
means updating `docs/wasmtime.md` and passing every check in §7 against the
new revision in the same change. Changing the fork itself is a maintainer
decision, taken before the change is written.

## 2. Construction over conditional compilation

- The kernel never selects a runtime, probes a feature, or switches behaviour
  on `#[cfg(target_arch)]`. Capabilities arrive through the backend's `Cpu`
  implementation, including executable-code publication and ISA feature
  probing.
- Kernel code depends on traits (`ComponentRuntimeFactory`, `HostFileSystem`,
  and their kin), never on concrete Wasmtime types. A runtime is an adapter
  that satisfies those traits.
- There is no global mutable state. State is owned and passed explicitly
  through `Kernel<CpuImpl>`, `RuntimeState<…>` and similar structures.
- A system capability is a Rust trait first and a WASI/WIT interface second.
  Define the trait, then expose it.

## 3. Code quality

### 3.1 Design

- No workaround, fallback, stub, or "simpler approach" code. When a design
  does not fit, fix the abstraction. When the only alternatives are a degraded
  variant or no change, make no change and record the blocker for a
  maintainer decision.
- An unexpected case fails fast with a message that names what was checked.
  Silently substituting a default (an emulator for an accelerator, a smaller
  budget, a skipped step) hides the defect and moves the failure somewhere
  harder to diagnose.
- No legacy or compatibility paths. Removing a feature removes every path
  that served it, in the same change.
- No architecture change while debugging. A failure that appears to involve a
  core subsystem is diagnosed in that subsystem, not routed around it.
- Memory bugs are never masked. Do not inflate memory sizes, widen budgets,
  disable limits, lower optimization, or add component-specific allocation
  tricks. When a program, component, or kernel plugin exhausts memory,
  diagnose the ownership boundary, allocator initialization, grow path and
  lifecycle, and fix the shared abstraction.
- Kernel memory and user memory are separate ownership domains. The kernel is
  as close to zero-allocation as practical and, when it must allocate, uses
  kernel-owned pools; kernel OOM is fatal and panics. User-mode wasm
  instances, kernel plugins included, use user-memory allocation and
  accounting; user OOM kills the instance and reclaims its pool, and never
  grows a kernel budget or adds per-plugin policy.
- Heap allocation in kernel-facing code needs a concrete reason tied to
  variable-sized guest data, plugin payloads, or runtime ownership. Stack
  storage, static capacity, caller-owned buffers, arenas and typed ownership
  come first.
- No dynamic dispatch in kernel-facing code. Generics, associated types and
  concrete adapters express a boundary whose implementations are known at
  compile time; `dyn Trait`, `Box<dyn …>`, `Arc<dyn …>` and type-erased
  callbacks do not appear.
- Virtio is the I/O path: boot, block, network, host share and debug
  transport. Replacing any of them with IDE, an emulated legacy device, an
  ad-hoc host file, or other fallback I/O is an architectural change that
  needs explicit approval in the task that proposes it.
- A well-maintained, `no_std`-friendly third-party crate beats a hand-rolled
  implementation.

### 3.2 Rust style

- `async fn` is the syntax for an async function in an impl block or at module
  level. A trait *declaration* spells its future out, with bounds, because
  that is where the contract lives:
  `fn op(&self) -> impl Future<Output = Result<T, E>> + Send + '_;`
  An implementation of that trait writes `async fn op(&self) -> Result<T, E>`;
  the declaration's bounds are checked against it. `fn … -> impl Future` in an
  implementation is correct only when the body returns another future
  untouched (a forwarder) rather than wrapping it in an `async` block.
  `clippy::manual_async_fn` enforces the impl-block half of this rule and
  is never allowed off.
- Errors are typed enums with `thiserror`; `anyhow` does not appear in this
  repository. A CLI or test boundary may translate an error into text, but
  every crate preserves structured provenance.
- Diagnostics go through `tracing`, the only diagnostic crate in the tree.
  The `log` crate is never used; a dependency that emits `log` records
  (`cranelift_codegen`, for one) is bridged with `tracing_log::LogTracer`.
  Kernel, `hal/`, library and protocol crates never call `println!`,
  `eprintln!` or `dbg!`. User-mode wasm programs under `programs/`, kernel
  plugins included, write to stdio with `println!` and `print!` as the
  natural thing; do not route them through the SDK for the rule's sake.
- Structured text is never built by string concatenation. A multi-line
  literal lives in a file pulled in with `include_str!`; templated output uses
  a compile-time typed template or a serde serializer for JSON, YAML and TOML,
  so a renamed field fails the build instead of emitting a broken document.
- Repeated module, enum and type paths get a `use`; long fully-qualified paths
  are not spelled inline.
- `#[path = "…"]` never pulls a source file across a crate boundary.
- Invariants live in types, not in runtime checks; structs, traits and
  generics express variation rather than enums or erasure.

### 3.3 Kernel plugins

- "Kernel plugin" is the term for an ordinary user-mode wasm program that
  runs inside Wasmtime under the normal isolation model. Plugins differ from
  other programs in provisioning and lifecycle, not in runtime boundary.
- Non-core kernel functionality may depend on kernel plugins.
- The compiler is a kernel plugin: bootfs-provisioned, loaded at kernel
  startup, trusted for signed `cwasm` output.
- The HTTP client is a kernel plugin (`programs/http-client`): the kernel
  implements `wasi:http/types` only and forwards `wasi:http/client.send`
  through a typed provider slot to the plugin's `wasi:http/handler`; HTTP/1.1
  framing, DNS and, later, TLS run in user memory. The provider slot is the
  route for any future interface a plugin serves.
- Plugins share the user-memory and allocation contract of every program.
  There is no plugin-private allocator policy, memory floor, or oversized
  linker memory. A plugin may fail with a typed OOM result and be discarded
  or restarted by its owner through the shared runtime contract.

### 3.4 Wasmtime runtime performance

- The kernel provides the exception and signal mechanism Wasmtime's runtime
  needs. `Config::signals_based_traps(false)` is never a final answer: it
  removes guard-page bounds-check elimination and Winch compatibility.
- Typed function references, SIMD, relaxed SIMD and target ISA feature
  probing stay correctly enabled wherever the real target supports them.
- Every Wasmtime performance opt-in the kernel can support stays enabled:
  pooling instance allocation, signals-based traps, epoch interruption,
  SIMD, threads, the component model, `async_stack_zeroing(false)`. With a
  real user VM, `memory_reservation` and `memory_guard_size` eliminate wasm32
  bounds checks, with `memory_init_cow(true)` and `memory_may_move(false)`.
  Disabling any of them needs a same-PR justification of the alternative
  semantic path.
- Full x86 AVX, FMA and AVX-512 enablement waits on the x86 kernel providing
  OSXSAVE, XCR0 configuration and XSAVE/XRSTOR state preservation, and is
  tracked as such.
- Performance decisions rest on bare-metal targets and explicit capabilities,
  never on `hosted/`.
- A performance test, benchmark, or acceptance measurement runs `--release`
  artifacts. Debug builds serve functional checks and diagnosis only.

### 3.5 Modern hardware, SMP first

Helios targets modern multi-core hardware. SMP correctness is a day-one
property of every kernel and `hal/` subsystem, never a follow-up.

- A new abstraction states its concurrency contract before its API: which
  operations are lock-free, which take an async mutex, which fan out to other
  processors. "Add SMP later" is not a design note.
- Every address-space mutation invalidates the local TLB and sends an IPI
  shootdown to every other processor that has run in that space.
- Hot paths use per-CPU storage indexed by `Cpu::current_processor()`;
  cross-processor queues prefer atomics and lock-free channels to a mutex.
  The processor index is only reachable where a `Cpu` is held: an executor,
  a scheduler, a service that was constructed with one. A task future, a
  drop path, or a global allocator holds none and never carries one (on
  every backend a `Cpu` is a refcounted handle, and cloning it per task is
  a cost on the spawn path), so a per-processor structure states which of
  its operations run on the owner and which arrive from any processor, and
  gives the latter a lock-free path that needs no processor index.
- Words written by different processors live on different cache lines
  (`crossbeam_utils::CachePadded`, sized per target), and a structure's
  docs say which processor writes each padded block.
- Hardware the target already has (XSAVE and AVX, MWAIT, cache-coherent I/O,
  GICv3) is brought up properly rather than avoided because its setup is
  SMP-aware.

### 3.6 Performance baselines

Performance is measured on one architecture: x86-64 Linux under KVM. Nearly
everything a Helios benchmark exercises lives in the cross-platform kernel,
so a second architecture would measure the emulator or the host, not the
kernel. `bench-x86-64-linux` is CI's only benchmark lane; the aarch64 and
riscv64 lanes are functional checks under TCG and never a performance
surface. GitHub's Arm runners expose no KVM, and macOS runners are not used
(§7).

Capture a baseline before any change that affects kernel-side runtime
performance, compare after, and cite the medians and any regression in the
PR. Baseline logs live under `target/perf-baselines/` and are not committed.
A developer laptop is not a benchmark host: take numbers from the CI lane or
a dedicated machine.

The canonical compute workload is the in-kernel compiler plugin compiling a
fixed wasm input. The regression target is the median `elapsed_ms` over
iterations 2..N; iteration 1 pays the cold cache build and is excluded.
`--compiler-timing` adds a Cranelift pass breakdown for diagnosis, not for
the baseline.

```bash
./target/release/helios-inspector vm --arch x86-64 --release --accel kvm \
    aot-bench artifacts/wasi-tools/curl.wasm --iterations 5 \
    | tee target/perf-baselines/x86-64-kvm-curl-<short-sha>.log
```

A network-path change takes a second baseline, because the compiler workload
never touches the NIC. The canonical network workload is `tcp-throughput` on
a multi-queue tap backend; slirp (`user`) is single-queue with no offload and
is not evidence for anything the virtio-net driver negotiates. Cite the
`virtio-net online` boot line beside the median: it records the queue-pair
count and the checksum and TSO bits the run actually had. `docs/networking.md`
covers the backends and the privileged setup.

```bash
helios-inspector vm net-setup \
    --net-backend tap --net-ifname helios0 --net-bridge helios-br0 --net-dhcp
./target/release/helios-inspector vm --arch x86-64 --release --accel kvm \
    --net-backend tap --net-ifname helios0 --net-bridge helios-br0 \
    --net-queues "$(nproc)" \
    workload-bench --workload tcp-throughput --iterations 5 \
    | tee target/perf-baselines/x86-64-tap-tcp-<short-sha>.jsonl
```

The same `aot-bench` on an arm64 machine under HVF (`--arch aarch64 --accel
hvf`) is a valid optional look at a second architecture. It is never required
and never comes from CI.

### 3.7 Architectural ambition

- Performance, scalability and cleanliness work is not cut short because the
  change is large. When an improvement tracks modern high-performance
  practice and yields a cleaner abstraction without breaking a contract here,
  take the full path: cross-crate interface reshapes, ownership moves across
  subsystems, ABI changes across backends.
- Effort is not a decision input. "Hours", "days", "quick win" and any other
  time-cost framing do not appear in a proposal. The criteria are whether the
  change resolves a root problem, whether the result tracks modern practice
  and reads more cleanly, and whether it introduces a contract violation.
- Large changes are discussed before they are built, to align on direction.
  That discussion is for alignment, never for permission to land a degraded
  variant.

## 4. Async-first execution

The kernel runs a cooperative async executor. Anything that pins it blocks
every other task: the 9p host-fs transport, WASI futures, timers, the network
service. These rules bind every `#![no_std]` crate and every crate the
executor drives (`hal/`, `kernel/`, the backends' runtime paths, components).

- `block_on` does not appear in production code. Its only uses are tests,
  bootstrap entry points that run before the executor starts, and its own
  definition. A caller that seems to need it becomes async instead.
- An async operation never looks synchronous. When an operation reaches a
  trait that implements it with `.await`, the public entry point is `async`
  or returns a future; no sync shim hides a `block_on`.
- Nothing busy-waits on software state inside an async context.
  `core::hint::spin_loop()` is for sub-microsecond hardware synchronisation
  only: a UART FIFO drain, a virtio descriptor completion, critical-section
  contention. Readiness, channel messages and transport replies are awaited
  through `Notify`, oneshot channels, or `yield_now().await`.
- A signal is armed before the condition is tested, never after, so that a
  wake-up between the test and the park cannot be lost. `Notify::notify_all`
  is a broadcast to waits that already exist; `notify_one` and
  `notify_count` store permits. The two never mix.
- Blocking interfaces (serial readers, channel receivers) expose a
  non-blocking `try_*` variant for async callers. Blocking variants exist for
  bootstrap paths only.
- No `Arc<Mutex<T>>` behind an async API. Channels, the kernel `Notify`, and
  single-owner `RefCell` within a task come first; when a lock is unavoidable
  it is the kernel's async `Mutex` or `RwLock` from `kernel/src/exec/sync.rs`,
  and the guard is released before an `.await`.
- A loop that polls for progress yields on the non-ready path so the executor
  can drive the task that produces the progress.

## 5. Inspector surface

- The inspector's commands are `shell`, `tracing`, `stats`, `repl` and `vm`.
  `repl` treats `stats` and `tracing` as local shortcuts and forwards
  everything else to the guest shell. `vm` boots the selected architecture
  under QEMU, waits for the guest debugger component, and then runs one of
  the session commands or a bench action: `aot-bench`, `workload-bench`,
  `net-setup`, `net-teardown`. No `qemu-shell` or `dash` entry returns.
- The accelerator is always named. With no `--accel`, the inspector requires
  the profile's native accelerator and fails saying which check refused it
  (`/dev/kvm` missing or unreadable, `kern.hv_support` not set, host
  architecture mismatch). Emulation is asked for with `--accel tcg`, never
  reached by falling back.
- `vm --arch aarch64 --acpi` boots the `virt` machine from ACPI tables
  instead of a device tree. The aarch64 kernel takes its whole platform
  description from firmware and QEMU publishes one description or the other,
  so both modes keep booting and CI runs both.
- `vm --kernel-debug --gdb <endpoint>` is the QEMU gdbstub path for
  symbol-level kernel debugging, with `--gdb-wait` to attach before kernel
  entry; `tcp::1234` works with GDB (`target remote :1234`) and LLDB
  (`gdb-remote 1234`). It stays working for `riscv64` and `x86-64`.
- `vm --debug` is the local kernel-debugging shortcut: debug profile, default
  gdbstub, wait for the debugger, retained runtime directory, monitor and QMP
  sockets. Prefer it to hand-written QEMU invocations.
- The inspector exposes the diagnosis knobs a developer would otherwise
  script by hand: retained runtime directories, QEMU stdout and stderr, QEMU
  `-d` traces, HMP and QMP sockets, CPU and accelerator overrides, raw QEMU
  argument passthrough. A missing knob is added to the inspector rather than
  repeated as a manual command.
- Boot and kernel debugging is evidence first: reproduce, capture serial and
  QEMU logs and GDB, LLDB or QMP state, and only then change boot, device
  topology, trap, or runtime code. A stuck or silent VM is inspected through
  the process, the gdbstub, the symbols, the serial socket and the QEMU logs
  before any conclusion; `hosted/` evidence does not stand in for it.
- Inspector and guest communicate through the WIT RPC defined in
  `helios-inspector-protocol`, never through a side channel.

## 6. WASI tooling

- `tools/wasi-apps/build.sh` is the single entry point that stages the WASI
  shared-fs tooling: it downloads the official CPython WASI build into
  `artifacts/python3-root/` (`python3.wasm` plus the standard library) and
  builds the Rust `curl-wasi` into `artifacts/wasi-tools/`.
- Helios tests against real CPython or nothing. A hand-rolled interpreter
  stub does not return.
- `docs/wasi-tools.md` describes the reproducible workflow; a change to
  runtime behaviour updates the docs and the paths in the same change.

## 7. Checks and CI

Before a change is complete, run the recipes for every surface it can
affect. `just check-target` and `just test-units` generate the
`helios-cli kernel-prebuild` manifest and pass it through
`HELIOS_KERNEL_PREBUILD_MANIFEST` themselves.

```bash
just check-host
just check-target aarch64-unknown-none helios-aarch64
just check-target riscv64gc-unknown-none-elf helios-riscv
just check-target x86_64-unknown-none helios-x86
just test-embedded-debugger
just lint
just test-units
```

`just lint` is `tools/fmt.sh --check` plus `cargo clippy … -D warnings` over
the host crates, each guest program, and the three bare-metal targets.
`just test-units` runs the `hal`, `virtio`, `netstack`, `kernel`,
`inspector-protocol` and `workspace-root` unit tests and the
`hal_layering` test that enforces §1.

CI (`.github/workflows/ci.yml`) runs the same recipes, one lane each, so a
red lane names the surface that broke:

| Lane | Runner | What it proves |
| --- | --- | --- |
| `check-host`, `check-aarch64`, `check-riscv`, `check-x86` | `ubuntu-24.04` | Every surface compiles. |
| `lint-fmt`, `lint-host`, `lint-aarch64`, `lint-riscv`, `lint-x86` | `ubuntu-24.04` | Formatting and clippy at `-D warnings`. |
| `test-units`, `test-embedded-debugger` | `ubuntu-24.04` | Unit tests and the embedded debugger. |
| `smoke-x86-64` | `ubuntu-24.04`, `--accel kvm` | Boot, shell, CPython, the in-kernel compiler, a trapped OOB load, curl over virtio-net, the raw serial captures. |
| `smoke-riscv64` | `ubuntu-24.04`, `--accel tcg` | Boot, shell, CPython, a trapped OOB load, the inspector RPC over vsock, curl over virtio-net. |
| `smoke-aarch64` | `ubuntu-24.04-arm`, `--accel tcg` | Boot from the device tree and from ACPI, against a pinned upstream QEMU the lane builds and caches, because Ubuntu 24.04's QEMU asserts in its emulated GICv3 under multi-threaded TCG. |
| `bench-x86-64-linux` | `ubuntu-24.04`, `--accel kvm`, multi-queue tap | The workload benchmarks of §3.6, with the negotiated virtio-net features reported. |

Every lane runs on a Linux runner. macOS runners are not used: their queue
dominates the run and nothing Helios ships targets macOS. Every boot in CI
names its accelerator explicitly, per §5.

The lint gate is `-D warnings` with no lint allowed workspace-wide; the
`[workspace.lints]` table exists so each crate can inherit it and is empty.
Suppressing a lint needs `#[expect(…, reason = "…")]` on the item, with a
reason that says why the lint is wrong there. `clippy::manual_async_fn` is
the gate for §3.2's `async fn` rule.

`dep-check.yml` runs on a schedule and on demand. `release.yml` runs
`release-plz` on every push to `main`: it owns version numbers, the
changelog and tags, so commits follow the conventional format (`feat:`,
`fix:`, `feat!:`, `BREAKING CHANGE:`) and nobody edits a version or a
changelog by hand.

## 8. Branches, pull requests, evidence

- Nobody pushes to `dev` or `main`. Work lives on a topic branch and lands
  through a pull request against `dev`. `main` is the release trigger, and
  merging into it is a maintainer decision.
- A problem becomes an issue before it becomes a branch; one issue, one
  branch, one PR, with `Fixes #N` on its own line in the body. A PR that
  advances an issue without closing it says `Part of #N`.
- Commits on `dev` are signed; the branch ruleset rejects unsigned ones.
  Commit messages and PR bodies carry no attribution trailers, generated-by
  footers, or session identifiers.
- A PR merges when every check it can affect is green. A red lane that is
  already red on `dev` and untouched by the PR gets its own issue, is named
  in the PR body, and does not block. Evidence in the PR body is concrete:
  the run id, the lane, the median, the negotiated feature line, the log
  line that proved the diagnosis.
- A change to the Wasmtime fork, to a CI runner or lane, to this file, or to
  anything that publishes is discussed with a maintainer before it is made.

## 9. Working in parallel

Several agents and developers work on this tree at once. The rules that keep
them from colliding:

- One checkout per task. A delegated agent works in its own git worktree
  branched from `origin/dev`, with a cloned `target/` for a warm cache, and
  never edits, cleans, or builds inside another worktree or another project.
- A delegated agent runs the per-crate checks for the crates it touched
  (`cargo check -p`, `cargo clippy -p … -D warnings`, the crate's tests, the
  relevant `just check-target`, file-scoped rustfmt). The workspace-wide
  lint and test suite is CI's job; running it locally as well pays the same
  compile twice.
- Benchmarks never run on a developer machine. The CI bench lane produces
  comparable artifacts; a laptop under other load does not.
- Waiting on CI is one bounded foreground command (`timeout 590 gh pr checks
  <N> --watch --interval 30`), not a polling loop. A wait that exceeds ten
  minutes is a defect signal: shorten the loop rather than wait longer.
- Every ordinary PR is one issue, one branch, one delivery. An agent that
  finds a second problem while fixing the first files a new issue and cites
  it, rather than widening the PR.
- Scratch files are shared between the agents of one session. A file an
  agent writes outside its worktree carries its branch or PR number in its
  name, and a PR body or issue body is written from a file named that way,
  never from a generic `pr.md`.
