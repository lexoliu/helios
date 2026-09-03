# Profile-guided optimisation feed

Issue #28 asks whether the benchmark suite can feed profile-guided
optimisation of (a) the bare-metal kernel and (b) the AOT compiler
plugin. This page records what was checked, on which toolchain, and the
verdict for each path. Nothing here was implemented: neither path is
straightforward, and the blockers below are filed as issues.

Toolchain examined: `rustc 1.98.0-nightly (3daae5e42 2026-06-14)`,
LLVM 22.1.6 (`rust-toolchain.toml`), vendored Wasmtime
`lexoliu/wasmtime@b83d18c8558b6d32fb0c0727d1c6a32639842c49`.

## (a) `-C profile-generate` for the bare-metal kernel

### What rustc needs

`-C profile-generate` makes LLVM emit counter updates into the
`__llvm_prf_cnts` section, metadata into `__llvm_prf_data`/`__llvm_prf_names`,
and references the `__llvm_profile_runtime` symbol. rustc satisfies that
symbol by injecting the `profiler_builtins` crate, which is the compiler-rt
profile runtime compiled by its `build.rs`
(`library/profiler_builtins/build.rs` in the toolchain's `rust-src`):
`InstrProfiling.c`, `InstrProfilingFile.c`, `InstrProfilingPlatformLinux.c`,
`InstrProfilingUtil.c`, `InstrProfilingWriter.c` and the rest. Those files
open files, read the environment (`LLVM_PROFILE_FILE`), call `uname`,
`fcntl` locks and `mmap`: they assume a libc.

Evidence on this toolchain:

- `lib/rustlib/aarch64-apple-darwin/lib/` ships
  `libprofiler_builtins-*.rlib`; `lib/rustlib/aarch64-unknown-none/lib/`
  ships none, and the same holds for `x86_64-unknown-none` and
  `riscv64gc-unknown-none-elf`. `-C profile-generate` on the kernel target
  therefore fails at link time for the missing runtime unless the crate is
  built with `-Zbuild-std`, and `build.rs` then needs a C toolchain that
  can compile compiler-rt for a target with no libc, which is where it
  stops.
- rustc has `-Z no-profiler-runtime` ("prevent automatic injection of the
  profiler_builtins crate", `rustc -Z help`). That is the door: the kernel
  can be instrumented without compiler-rt if Helios provides its own
  runtime.

### What an in-kernel runtime would be

1. Build the kernel with `-C profile-generate -Z no-profiler-runtime`
   (per-target `rustflags` in `.cargo/config.toml`), defining
   `__llvm_profile_runtime` in `kernel/` so the linker is satisfied. The
   instrumentation itself is target-independent: counters are plain
   loads/stores into `__llvm_prf_cnts`.
2. Keep the sections: `link.x` currently lists no `__llvm_prf_*` input
   sections, so they would be placed by default rules and could be
   dropped by `--gc-sections`. The linker script needs `KEEP` for
   `__llvm_prf_cnts`, `__llvm_prf_data`, `__llvm_prf_names`,
   `__llvm_prf_vnds` (and `__llvm_prf_bits` for the newer coverage bits)
   with `__start_`/`__stop_` symbols for each.
3. Write a `.profraw`: a header (magic, version, counter/data/name section
   sizes and deltas), followed by the raw section bytes, in the format
   LLVM 22's `llvm-profdata` reads. The format carries a version field
   and changes between LLVM releases; the writer must assert the version
   it implements against `rustc -vV`'s LLVM.
4. Get it out of the guest. Two transports exist today: the debugger's
   `helios:system/profiling` interface, which streams folded samples to the
   inspector over the serial or vsock RPC (`docs/inspector-vsock.md`), and
   the host share. A `profiling.llvm-raw-profile` call returning the bytes
   fits the existing RPC and needs no filesystem; the inspector writes the
   file and runs `llvm-profdata merge`.
5. Size: one 8-byte counter per instrumented region. The kernel links
   Wasmtime, Cranelift and the netstack, so the counter section is in the
   low tens of megabytes for a `--release` kernel; it must live in
   kernel-owned memory that is reserved at link time, not allocated at
   runtime, and it doubles as a fixed cost on every instrumented boot.
6. Compile-time counter updates in the kernel's hot paths (the executor,
   the virtio queues, the netstack's per-packet path) are not free and
   change scheduling; the suite would run the instrumented kernel only to
   collect profiles, never to report numbers.

Verdict: feasible, not straightforward. It is a kernel feature (a
runtime, a linker-script change, a WIT call, an inspector command and a
CI job), not a benchmark-tooling change, and every piece is pinned to the
LLVM raw-profile version.

### Sample-based alternative already within reach

rustc also has `-Z profile-sample-use=<file>` (AutoFDO). The kernel already
has a sampling profiler that exports folded stacks with weights
(`helios:system/profiling.folded`, `workload-bench --kernel-profile-output`).
AutoFDO needs a per-source-line sample profile, which the folded output
does not carry (it is symbol-level), so this path needs either
instruction-address samples with DWARF line mapping from the kernel's
profiler or LBR-style data, neither of which the profiler records today.

## (b) Cranelift-level feedback for the AOT compiler plugin

Checked in the vendored tree (`cranelift/codegen`, `cranelift/frontend`,
`crates/cranelift`):

- Cranelift has no profile input: no counter instrumentation pass, no
  block-frequency import, no `ProfileData`. `wasmtime::Config::profiler`
  (`ProfilingStrategy`: jitdump, perfmap, VTune) exports symbols for host
  profilers; it consumes nothing.
- The only feedback channel that exists is the wasm **branch hinting**
  proposal: `crates/cranelift/src/translate/code_translator.rs` marks the
  unlikely successor of a hinted `if` cold (`builder.set_cold_block`) and
  the block-order pass moves cold blocks out of line
  (`cranelift/codegen/src/machinst/blockorder.rs`).

So the feedback the compiler plugin can consume today is a
`metadata.code.branch_hint` custom section in the input wasm. Producing
it from the suite means instrumenting a wasm module for branch counts,
running it on Helios under the suite, and writing the hints back into the
module before the plugin compiles it. Neither the instrumentation nor the
writer exists in this repository or in the vendored Wasmtime; Binaryen's
branch-hint passes are the natural producer, and Binaryen is not a
dependency of this repository. Any wasm shipped with hints must be
re-hinted whenever it is rebuilt, which ties the compute-parity artifacts
(CPython, QuickJS) to a hinting step in `tools/wasi-apps/build.sh`.

Verdict: the channel is real and cheap to consume (Cranelift already
does), but the producer is a new tool chain step and its effect on
Cranelift's output is limited to block layout. Not implemented.

## Issues filed

- #70: in-kernel `-C profile-generate` runtime with `-Z no-profiler-runtime`,
  the linker-script sections, the raw-profile writer and the export call.
- #71: branch-hint feedback for the compiler plugin: instrumenting the
  suite's wasm inputs, writing `metadata.code.branch_hint`, and re-hinting
  in `build.sh`.
