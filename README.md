# Helios

An experimental Rust kernel that runs user programs as WebAssembly components.

Helios boots on bare metal, exposes system capabilities through WIT interfaces,
and executes user-space code inside Wasmtime with the WASI Preview 3 component
model as its only ABI. The kernel itself is architecture-neutral; concrete
hardware support lives in thin adaptation crates.

## Highlights

- **WASI Preview 3 as the syscall surface.** System services
  (`helios:system/programs`, `net`, `stats`, `tracing`, `sync`, `serial`,
  `instances`) are defined as WIT interfaces first, then implemented against
  Rust traits inside the kernel.
- **Architecture-neutral kernel.** `helios-kernel` is `#![no_std]` and generic
  over a `Cpu` implementation supplied by the backend crate. No `#[cfg(target_arch)]`
  branches leak into core logic.
- **Async-first runtime.** A small cooperative executor built on `async-task`
  drives futures across processors, with a priority-aware compute pool for
  Wasmtime compilation work.
- **Capability-based resources.** `KernelResource<T, Rights>` carries its own
  rights set; derived handles can only narrow permissions, never widen them.
- **Three backends.**
  - `helios-riscv` — RISC-V 64 (`riscv64gc-unknown-none-elf`), SBI-based boot,
    virtio-mmio transport, ns16550a UART.
  - `helios-x86` — x86_64 (`x86_64-unknown-none`), `bootloader_api` entry,
    COM1 serial, PIT-calibrated timer.
  - `helios-hosted` — runs the kernel on top of the host OS for fast iteration
    and testing.
- **Inspector tooling.** `helios-inspector` is a host-side CLI that speaks the
  same WIT interfaces over a serial transport, so a guest running in QEMU is
  observable and controllable through the same contracts the kernel exposes
  to its own components.

## Workspace layout

| Crate | Role |
| --- | --- |
| `hal/` | Architecture-neutral hardware traits (`Cpu`, `FileSystem`, `NetDriver`, …) |
| `kernel/` | Hardware-independent runtime: executor, timer, compute pool, component host, capability resources |
| `riscv/` | RISC-V 64 backend (boot, trap, virtio, UART) |
| `x86/` | x86_64 backend (bootloader entry, SMP wakeup, serial, timer) |
| `hosted/` | Hosted backend that runs the same kernel on a normal OS |
| `api/` | Async userland SDK for programs built as wasm components |
| `api-macro/` | `#[helios_api::main]` proc macro |
| `kernel-macro/` | Kernel-side proc macros |
| `programs/init/` | Embedded init component |
| `programs/sh/` | Minimal shell component |
| `programs/debugger/` | Debugger component that exports `helios:system/*` back over RPC |
| `inspector/` | Host-side CLI (`shell`, `stats`, `tracing`, `repl`, `vm`) |
| `inspector-protocol/` | Shared RPC types between inspector and guest debugger |
| `virtio/` | virtio device type definitions shared across backends |
| `wit/` | Helios WIT worlds and WASI dependency snapshots |

## Building

Helios targets the 2024 edition and a recent nightly toolchain (WASI Preview 3
and component-model features are still evolving upstream). From the workspace
root:

```bash
# Hosted backend (fastest iteration)
cargo check -p helios-hosted

# RISC-V 64 bare-metal backend
cargo check -p helios-riscv --target riscv64gc-unknown-none-elf

# x86_64 bare-metal backend
cargo check -p helios-x86 --target x86_64-unknown-none

# Architecture-neutral kernel
cargo check -p helios-kernel

# Inspector CLI
cargo check -p helios-inspector
```

Helios currently pins a local Wasmtime checkout through a `path = "../wasmtime/..."`
workspace dependency so the kernel can track Component Model / WASI 0.3 changes
ahead of crates.io releases. Clone Wasmtime next to this repository before
building.

## Running

The inspector launches a guest under QEMU and attaches over the guest's
debugger component:

```bash
# Boot the RISC-V guest
cargo run -p helios-inspector -- vm --arch riscv64

# Boot the x86_64 guest (BIOS entry)
cargo run -p helios-inspector -- vm --arch x86_64
```

Once attached, the inspector exposes five commands over the same WIT surface
that guest components use internally:

- `shell` — execute a command remotely via the guest shell
- `stats` — live TUI of kernel statistics
- `tracing` — stream filtered trace events
- `repl` — combined shell + stats + tracing session
- `vm` — boot a QEMU guest and drop into a `repl` automatically

See `docs/wasi-tools.md` for the reproducible workflow that builds and runs
upstream WASI tools (Python, curl) against a shared host directory.

## Status

Helios is an experimental personal project. Interfaces, worlds, and the
Wasmtime pin are expected to move frequently. There is no stability promise
on any public surface at this point.
