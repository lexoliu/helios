# Profile-guided optimisation feed

Issue #28 asks whether the benchmark suite can feed profile-guided
optimisation of (a) the bare-metal kernel and (b) the AOT compiler
plugin. This page records what was checked, on which toolchain, and where
each path stands. Path (a) is implemented and is described below as it
works today; path (b) is not, and its blocker is filed as an issue.

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

### The in-kernel runtime

`-C profile-generate` on the kernel is a build profile, a runtime in
`kernel/`, a linker-script fragment per target, an export and a CI job.
Every piece is below, and none of them exists in a plain kernel: the
runtime and the writer are compiled only under `--cfg
helios_profile_generate`, the linker fragments are passed only by the
instrumented build, and a plain kernel answers the export with
`not-instrumented` rather than an empty profile a merge would take for a
workload that ran nothing.

#### Building one

```bash
just build-instrumented x86-64      # aarch64, riscv64, x86-64
helios-inspector vm --arch x86-64 --profile-generate --accel kvm shell
```

`just build-instrumented` is the inspector's own `vm --profile-generate
build`, so the flags have one definition (`inspector/src/vm.rs`,
`profile_generate_rustflags`) and a check and a boot cannot disagree
about what an instrumented kernel is. The build differs from `--release`
by the `profile-generate` cargo profile — release, in a target directory
of its own — and by four rustflags:

| Flag | Why |
| --- | --- |
| `-C profile-generate` | emits the counters and the `__llvm_prf_*` sections |
| `-Z no-profiler-runtime` | keeps rustc from injecting compiler-rt's runtime, which assumes a libc |
| `-C llvm-args=-disable-vp=true` | turns value profiling off, see below |
| `--cfg helios_profile_generate` | compiles the kernel's own runtime, in the same flags that emit the instrumentation |

They arrive as one `cargo --config` override rather than through
`RUSTFLAGS`, because cargo *joins* a `--config` array with the one
`.cargo/config.toml` sets for the same target while the environment
variable would *replace* it, costing the target its link arguments and
its ISA features.

Value profiling is off deliberately. It calls
`__llvm_profile_instrument_target` on every indirect call, allocates a
node per new call target, and makes the profile's length depend on what
has executed — which the size-then-window export below could not
describe without freezing the guest. The profile is therefore a
counter profile: `llvm-profdata` reads it, `-C profile-use` consumes it,
and the one optimisation it cannot drive is indirect-call promotion.

#### The runtime, in `kernel/src/profiling`

`__llvm_profile_runtime` is defined there — a definition is all any
instrumented object wants of it — and so is the `.profraw` writer. The
module allocates nothing: the counters live in the sections the linker
reserved, and the writer serialises the image a window at a time out of
them.

The format is pinned to the toolchain, not guessed at. Every object
rustc instruments carries `__llvm_profile_raw_version`, the version word
LLVM's own runtime would have written; the writer refuses to produce a
byte unless it equals the version it implements — 10, with the
IR-instrumentation variant bit, on the pinned nightly's LLVM 22.1.6.
A toolchain bump that changes the raw format therefore fails loudly at
the export instead of writing a file `llvm-profdata` misreads.

#### Keeping the sections

`aarch64/profile-generate.ld` and `riscv/profile-generate.x` place
`__llvm_prf_data`, `__llvm_prf_names`, `__llvm_prf_vnds`,
`__llvm_prf_bits` and `__llvm_prf_cnts` with `KEEP` and define a
`__start_`/`__stop_` pair for each. Both targets bring their own linker
script, so the sections have to be placed: on riscv64 an orphan would
land outside `__sdata .. __edata`, the window `_start` copies from the
image, and the per-function records — which carry link-time relative
pointers, not zeroes — would never reach RAM. The fragments are added
with an extra `-T` by the instrumented build alone, so a plain image is
byte-for-byte what the base script produces.

`x86_64-unknown-none` has no fragment and needs none: it links with
LLD's own layout, which places the sections as ordinary orphans, keeps
them under `--gc-sections` because their `__start_`/`__stop_` symbols are
referenced (LLD's default `-z nostart-stop-gc`), and synthesises those
symbols itself.

#### Getting it out of the guest

`helios:system/profiling` gained two calls, `raw-profile-size` and
`raw-profile-read`, and the guest debugger forwards both over the
existing inspector RPC. The length is fixed by the link rather than by
what has executed, so a reader asks once and then walks the image while
the kernel keeps counting — the same property that lets compiler-rt dump
a profile from a running process. Each read is capped
(`helios_kernel::MAX_PROFILE_READ`), which bounds the one transient
buffer the export lowers into the guest.

On the host:

```bash
# collect and stop
helios-inspector vm --arch x86-64 --profile-generate --accel kvm \
    profile target/pgo/boot.profraw

# collect after a workload
helios-inspector vm --arch x86-64 --profile-generate --accel kvm \
    aot-bench artifacts/wasi-tools/curl.wasm --iterations 2 \
    --llvm-raw-profile-output target/pgo/aot-bench.profraw
```

Both write the raw profile and run `llvm-profdata merge` beside it. The
tool is looked up before the guest is asked for a byte, and a host
without it is told exactly what is missing: `rustup component add
llvm-tools` puts `llvm-profdata` in
`$(rustc --print target-libdir)/../bin`, and its LLVM is the one that
matches the instrumentation.

#### Collecting in CI

`bench-suite.yml` has a `profile-generate` job: it builds the
instrumented x86-64 kernel, runs the compiler workload and the suite's
non-network classes on it under KVM, merges every `.profraw` and uploads
one `helios-kernel.profdata`. It reports no numbers and it is not a
benchmark surface — counter updates in the executor, the virtio queues
and the netstack's per-packet path change scheduling, so anything timed
on an instrumented kernel would be measuring the counters. The network
class is not in the profile yet: it needs the privileged tap backend the
suite lane provisions.

Consuming the profile with `-C profile-use` is the next step and is not
wired up: the collected artifact is what makes it possible to try.

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
  Implemented; described above.
- #71: branch-hint feedback for the compiler plugin: instrumenting the
  suite's wasm inputs, writing `metadata.code.branch_hint`, and re-hinting
  in `build.sh`.
