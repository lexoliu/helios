# Profile-guided optimisation feed

Issue #28 asks whether the benchmark suite can feed profile-guided
optimisation of (a) the bare-metal kernel and (b) the AOT compiler
plugin. This page records what was checked, on which toolchain, and where
each path stands. Path (a) is implemented and is described below as it
works today; path (b) is not, and its blocker is filed as an issue.

Toolchain examined: `rustc 1.98.0-nightly (3daae5e42 2026-06-14)`,
LLVM 22.1.6 (`rust-toolchain.toml`), vendored Wasmtime
`lexoliu/wasmtime@b83d18c8558b6d32fb0c0727d1c6a32639842c49`.

## (a) Profile-guided optimisation of the bare-metal kernel

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

### Collecting it: the in-kernel runtime

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

#### What it costs

Measured on the pinned toolchain, booting each instrumented kernel to the
debugger and collecting straight away:

| Target | Instrumented functions | Counters | Raw profile |
| --- | --- | --- | --- |
| `aarch64-unknown-none` | 49,874 | 267,889 | 6,147,776 B |
| `x86_64-unknown-none` | 43,065 | 246,687 | 5,580,768 B |
| `riscv64gc-unknown-none-elf` | 41,675 | 246,376 | 5,403,776 B |

The counters are a fixed cost on every instrumented boot — a little over
two megabytes of link-time memory — and the profile is small enough to walk
out over the RPC in a few seconds.

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

### Spending it: `-C profile-use`

`vm --profile-use <file>` is the other half. It is the release build plus
two rustflags, in a cargo profile and a target directory of its own, so a
PGO image and a plain one can be built from one checkout without
overwriting each other — which is what lets the two be timed against each
other.

| Flag | Why |
| --- | --- |
| `-C profile-use=<file>` | reads the merged profile; the path is an argument, never discovered |
| `-C llvm-args=-pgo-warn-missing-function` | names every function the profile says nothing about, as a warning |
| `-C llvm-args=-disable-vp=true` | the collection turns value profiling off, so the use side has to agree |

```bash
just kernel-pgo-use x86-64 target/pgo/helios-kernel.profdata
helios-inspector vm --arch x86-64 \
    --profile-use target/pgo/helios-kernel.profdata --accel kvm shell
```

The last flag is the collection's own, restated. The instrumented build
turns value profiling off (above), so every record carries zero value
sites; a default use build expects as many as the function has indirect
calls, and reports each mismatch as "inconsistent number of value sites
… possibly due to the use of a stale profile" — a wrong diagnosis of a
correct profile, three hundred times over on the x86-64 kernel. The two
halves state the same thing about value profiling, or they disagree about
what the profile contains.

`just kernel-pgo-use` is the inspector's own `vm --profile-use build`,
the way `just build-instrumented` is `vm --profile-generate build`, so
the flags have one definition (`inspector/src/vm.rs`,
`profile_use_rustflags`).

The profile is an explicit argument on purpose: a PGO kernel is only as
good as the profile behind it, so which profile that was is part of the
command that built it and part of the run record of anything timed on it.
`--profile-use` composes with `--release` and with nothing else:
`--profile-generate`, `--debug` and `--kernel-debug` are refused by name,
because a build cannot both collect a profile and read one, and an
unoptimised PGO build would measure neither.

#### Refusing a profile the toolchain cannot read

The build checks the profile's header before cargo starts
(`inspector/src/vm/profdata.rs`), the way the guest writer checks
`__llvm_profile_raw_version` before it writes a byte. Sixteen bytes
answer three questions:

- a `.profraw` reaching `--profile-use` means the `llvm-profdata merge`
  step was skipped, and is refused naming that step rather than "not an
  indexed profile";
- an indexed profile whose version is not the one this toolchain writes
  is refused naming both versions. On the pinned nightly that is 13
  (`IndexedInstrProf::ProfVersion::CurrentVersion`, LLVM 22.1.6), the
  index-side counterpart of the raw version 10 the guest writer emits;
- a profile without the IR-instrumentation variant bit did not come from
  `-C profile-generate` and is refused as such.

Without the check a stale artifact fails twenty minutes into a kernel
build, with an LLVM error that names no file.

#### What a stale profile costs

Nothing fails. LLVM matches a profile record to a function by its
mangled name and by a hash of its control-flow graph, and reacts to a
miss quietly:

- **a function that is not in the profile at all** — added since the
  collection, or never called by a collected workload — keeps its
  static branch heuristics and its default inlining. It is the state
  every un-instrumented build is in, so the cost is the optimisation not
  gained rather than a pessimisation. `-pgo-warn-missing-function` is
  what makes those visible, as warnings: they never fail the build.
- **a function whose control flow changed** since the collection — an
  added branch, a changed loop — has a record under its name whose hash
  no longer matches. LLVM discards that record (`instr_prof_hash_mismatch`)
  and the function falls back to the same static heuristics. The danger
  is not the discard but its silence at scale: a profile stale enough
  that most hashes miss produces a kernel that is a release build wearing
  a PGO label.
- **counts that are merely old** — the function and its shape are
  unchanged but the workload mix has moved — are the case with no
  diagnostic at all. A branch that was cold when the profile was taken is
  laid out out-of-line and stays there. This is what makes the paired
  measurement below the only real check: a profile that no longer
  describes the kernel shows up as no improvement, not as an error.

So the profile is refreshed by re-running the collection, never by
patching; and the artifact carries the run that produced it.

#### Measuring it in CI

`bench-suite.yml` has a `suite-pgo` job after `profile-generate`, on the
same events. It downloads that run's `helios-kernel-profdata`, builds the
candidate kernel with `--profile-use` and the baseline kernel plain from
the same commit, and runs the paired suite of #173 with the two on one
host: `--sides helios,helios_baseline`, which is Helios against Helios,
because the Linux sides answer a different question and would double a job
that already boots every workload twice.

The pairing machinery varies one thing between its two columns. Until now
that was the commit — a baseline worktree of another ref (#173, #178) —
and here it is the build: one commit, two kernels, and
`tools/bench/pgo-gate-note.md` beside the gate table saying which column
read the profile. The report is the paired table and the per-workload
medians; read the headline compute workloads first (`aot-curl`,
`cpython-json`, `quickjs-loop`), because they are what the profile covers.

The `net` class is timed on both images and is **not in the profile**: the
collection job runs the classes that need no privileged host networking,
so the candidate's packet path carries no counts. Profiling it needs a
collection run on a lane that provisions the tap backend.

The job reports and does not gate. Its verdict is about profile-guided
optimisation of the kernel, not about the pull request that happened to
run it, so a red headline row says PGO did not pay on this commit — which
is the answer the job exists to produce.

Shipping a PGO kernel from `release.yml` is the next step, and it is
decided from these numbers rather than before them: it needs a
profile-refresh cadence, which the paragraphs above are the argument for.

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
- #211: `-C profile-use` for the kernel, the `profile-use` build kind and
  the paired `suite-pgo` job. Implemented; described above.
