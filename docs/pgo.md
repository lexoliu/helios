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

`kernel-profile.yml` is the collection job (#313): it prepares a bench
host and then runs `.github/actions/collect-kernel-profile`, which builds
the instrumented x86-64 kernel, runs the compiler workload and the
suite's non-network classes on it under KVM and merges every `.profraw`
into one `helios-kernel.profdata`; the job uploads that as the
`helios-kernel-profdata` artifact, which is the profile every x86-64
release build spends (#226). It runs on `workflow_dispatch`, and on the
first of every month so the artifact never ages past GitHub's ninety-day
retention. `bench-suite.yml`'s `profile-generate` job calls the same
workflow, so the profile `suite-pgo` measures is collected by the job a
release build reads from; the collection itself is a composite action
because the release job below runs the same one on the released tree.
It boots the lane's own machine, read from
`tools/bench/manifest.toml`: the collection runs the lane's workloads, and
a guest smaller than the one the suite times cannot run them — `4G`
against the lane's `6G` took the x86-64 kernel's memory pool down on
`process-startup` (run 34011609558, job 101428527454). It runs
`--keep-going` for the same reason the suite does: `instance-startup-500`
wants more guest than the lane has, and a workload this machine cannot run
is a recorded failure rather than the end of the pass — the profile is the
counts of everything that did run, and stopping there would collect
nothing for the classes behind it. It reports no numbers and it is not a
benchmark surface — counter updates in the executor, the virtio queues
and the netstack's per-packet path change scheduling, so anything timed
on an instrumented kernel would be measuring the counters. The network
class is in the profile: the collection provisions the tap backend the
suite lane times it on and boots the instrumented kernel with it (#315).

### Spending it: `-C profile-use`

`vm --profile-use <file>` is the other half. It is the release build plus
two rustflags, in the `profile-use` cargo profile.

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

#### Where each build lands

One cargo profile is one output directory, and on x86-64 the release
kernel is a `profile-use` build too (#226), so the directory alone no
longer says which kernel is which. What a build reads decides where it
goes:

| Build | Directory | Image |
| --- | --- | --- |
| `--release` on x86-64, reading the fetched profile | `target/x86_64-unknown-none/profile-use/` | `helios` |
| `--release --without-kernel-profile`, the plain control | `target/x86_64-unknown-none/release/` | `helios` |
| `--profile-use <file>` | `target/pgo-kernels/<digest of the profile>/x86_64-unknown-none/profile-use/` | `helios` |

The named build gets a `--target-dir` of its own, keyed by the SHA-256 of
the profile it reads, because it is the same cargo profile as the release
kernel and the two would otherwise be one file: whichever built second
overwrote the first, and a paired run booted one image twice (#327). The
key is the profile's bytes rather than its path for the same reason the
directory exists at all — cargo fingerprints the rustflag that names the
profile and never the bytes behind it, so a profile rewritten under a
name that has been built against before would reuse the objects compiled
against the profile it replaced.

A `--target-dir` rather than a cargo profile of its own, for two reasons.
A cargo profile is written into `Cargo.toml`, so there is one of them
however many profiles a checkout weighs: two named profiles would land in
it together and be the same collision one directory down. And a profile
of its own is a second set of optimisation settings to keep in step with
`profile-use`, where the whole claim of a PGO pairing is that the two
columns differ by their profile and by nothing else. The release kernel
keeps cargo's default directory and its cache: the fetched-profile build
is the one that runs on every release lane and on every `--release` boot
of this target, and it is untouched by a named profile arriving beside
it.

The guest programs, the compiler plugin and the signed `cwasm` bootfs are
not built against the kernel's profile, so the prebuild stays where it is
(`target/kernel-prebuild/<target>/profile-use/`) and both columns of a
pairing carry the same one: what varies between them is the kernel's own
code generation and nothing else.

Nothing reconstructs these paths. `vm … kernel-path` prints the image a
set of flags resolves to and `vm … build` prints the image it produced;
the paired driver and `release.yml` both ask rather than spell a path out
(`inspector/src/vm.rs`, `KernelBuildSpec::target_dir`).

The profile is an explicit argument whenever two profiles are being told
apart: a PGO kernel is only as good as the profile behind it, so which
profile that was is part of the command that built it and part of the run
record of anything timed on it. The one profile that needs no argument is
the release's, below: `--release` on x86-64 reads it, and naming
`--profile-use` overrides it.
`--profile-use` composes with `--release` and with nothing else:
`--profile-generate`, `--debug` and `--kernel-debug` are refused by name,
because a build cannot both collect a profile and read one, and an
unoptimised PGO build would measure neither.

#### Refusing a profile the toolchain cannot read

The build checks the profile's header before cargo starts
(`helios-profdata`, the crate the inspector and `helios-cli` share),
the way the guest writer checks
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
candidate kernel with `--profile-use` on it, builds the baseline kernel
the way a plain `--release` build of this lane is built — which is
against the fetched profile, the newest collection on the default
branch — and runs the paired
suite of #173 with the two on one host: `--sides
helios,helios_baseline`, which is Helios against Helios, because the
Linux sides answer a different question and would double a job that
already boots every workload twice.

That pairing is what "refresh at release time" has to be measured
against. Both columns are `profile-use` builds of one commit and what
varies between them is the profile: the release's counts against the
counts this run collected. The candidate boots
`target/pgo-kernels/<digest>/x86_64-unknown-none/profile-use/helios` and
the baseline `target/x86_64-unknown-none/profile-use/helios`, per the
table above; before they had two directories the job built both into the
second one and the identical-images guard refused the run (#327). A
candidate that does not beat the baseline says the release's profile
still describes this kernel; one that does
says the profile has aged, which is the argument for cutting the next
release's collection. The run record names each column's profile
(`kernel_profile`, `baseline_kernel_profile`) and the paired table's
labels carry them, because two `profile-use` builds of one commit are
otherwise indistinguishable.

The pairing machinery varies one thing between its two columns. Until now
that was the commit — a baseline worktree of another ref (#173, #178) —
and here it is the profile: one commit, two kernels, and
`tools/bench/pgo-gate-note.md` beside the gate table saying which column
read the profile. The report is the paired table and the per-workload
medians; read the headline compute workloads first (`aot-curl`,
`cpython-json`, `quickjs-loop`), because they are what the profile covers.

The `net` class is in the profile like every other: the collection
provisions the tap backend and runs the whole suite on it, so the
candidate's packet path carries counts from the workloads the table times
it on (#315).

The job reports and does not gate. Its verdict is about profile-guided
optimisation of the kernel, not about the pull request that happened to
run it, so a red headline row says PGO did not pay on this commit — which
is the answer the job exists to produce.

What profile-guided optimisation is worth on the kernel that ships is a
different question, and it has a control: the same commit built without
any profile. `helios-inspector vm --release --without-kernel-profile`
builds it, on the one target whose release builds otherwise read the
fetched profile, and puts it in the `release` directory where every
other target's release kernel lands; asking for it elsewhere is refused,
because there the plain build is the only build. `helios-bench run
--baseline-kernel-build release` pairs the profile-guided kernel against
that control in one job (`bench-suite.yml`'s `baseline_kernel_build`
input), and the report says so: `kernel_profile` names the candidate's
profile and `baseline_kernel_build` is `release` (#322). That pairing is
the before/after every landed profile is measured by.

#### Shipping it in a release

A release carries the profile its kernel was built with (#226).
`release.yml` reads the tag release-plz cut for the `helios` package out
of the action's own `releases` output — the kernel image is that package,
and a release run that bumped a library alone cut no kernel release and
collects nothing — checks that tree out, runs the collection, builds the
released kernel with `--profile-use` against what it collected, and
attaches two assets to the release under stable names:

| Asset | What it is |
| --- | --- |
| `helios-kernel.profdata` | the merged profile, collected on the released commit |
| `helios-kernel-x86-64` | the released x86-64 kernel, built with `-C profile-use` on it |

The collection is one sequence for both workflows:
`.github/actions/collect-kernel-profile` is what the `profile-generate`
job of `bench-suite.yml` runs and what the release job runs. A profile
collected two ways would be two profiles wearing one name, and the kernel
a release ships would not be the kernel `suite-pgo` measured.

The refresh cadence is the on-demand collection above, not the release:
a profile goes stale in its counts long before it goes stale in its
hashes, and silently, so `kernel-profile.yml` is dispatched when the
kernel it describes has moved and runs monthly regardless. A release
attaches the profile its own kernel was built with so that the released
image can be reproduced, and a release build between releases reads the
newest collection on the default branch.

A release published before this job existed carries no profile.
Dispatching `release.yml` with its `tag` input names such a release and
attaches the two assets to it; a tag with no release behind it is refused
by name, before the collection rather than after it.

#### Spending it: `helios-cli profile-fetch`

Every x86-64 release build reads a fetched profile, so the kernel a
developer boots with `--release`, the kernel the smoke and bench lanes
measure, and the kernel a release ships are one build.

```bash
helios-cli profile-fetch                 # the newest collection on the default branch
helios-cli profile-fetch --branch perf/x  # the newest collection on a branch
helios-cli profile-fetch --tag helios-v0.1.0   # the asset a release carries
helios-inspector vm --arch x86-64 --release --accel kvm shell
```

The fetch is the one entry point that puts a profile in the store.
Without `--tag` it lists the repository's `helios-kernel-profdata`
artifacts through GitHub's API, takes the newest unexpired one whose run
was on the default branch (`kernel-profile.yml`, #313), follows the
archive redirect to GitHub's object store with curl and reads
`helios-kernel.profdata` out of the archive; with `--tag` it reads the
release and downloads the asset `release.yml` attached. Either way it
checks the header before the file counts as fetched, and records where
the profile came from:

```text
target/profiles/fetched.json                    the profile in force
target/profiles/run-<id>/helios-kernel.profdata  a collection, keyed by its run
target/profiles/<tag>/helios-kernel.profdata     a release, keyed by its tag
```

The record labels the profile the way the paired table will
(`dev@85d20bc run 34424416974`, or `release helios-v0.1.0`), so a kernel
image can be traced to the counts that shaped it. The store is under
`target/` because a checkout reproduces it by fetching again;
`helios-profdata` owns both the header check and the store, so the tool
that writes it and the tool that reads it hold one definition of what a
profile is and where it lives. The API and the archive need a token
(`GITHUB_TOKEN`, or `GH_TOKEN` as `gh auth` sets it); a lane's is the
job's own, with `actions: read`.

Nothing about the path is discovered and nothing falls back. No
collection on the branch, an expired one, a release without the asset, a
header this toolchain cannot read, or an empty store at build time is a
refusal that names what was checked and the command or the workflow that
would fix it. A release build that quietly dropped the profile would
produce exactly the kernel §"What a stale profile costs" warns about: a
release build wearing a PGO label.

Only x86-64 reads a profile this way. Performance is measured on one
architecture (AGENTS.md §3.6) and it is the one whose releases carry a
profile; on the others a release build is a release build, because a
`.profdata` carries the function hashes of the target it was collected
on and there is none for them to spend. The inspector says which target
does in one field of its target table (`release_kernel_profile`), not in
a `cfg`.

Three CI lanes therefore fetch before they build: `smoke-x86-64`,
`bench-x86-64-linux` and `bench-suite.yml`'s `suite`. Until a
`kernel-profile.yml` run on the default branch has uploaded the artifact,
every one of them fails at the fetch, and the message names the workflow
to dispatch.

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
  unlikely successor of a hinted `if` or `br_if` cold
  (`builder.set_cold_block`) and the block-order pass moves cold blocks out
  of line (`cranelift/codegen/src/machinst/blockorder.rs`).

So the feedback the compiler plugin can consume is a
`metadata.code.branch_hint` custom section in the input wasm, and the
producer is this repository's own: `tools/branch-hints`
(`helios-branch-hints`), three subcommands that close the loop from a real
run back into the artifact. Binaryen has branch-hint passes and would have
been the other candidate; it is not a dependency of this repository, and
the counting side — running the instrumented module *on Helios* and getting
its counters back — is Helios-specific either way.

`helios-compiler-support` turns the channel on with
`Config::wasm_branch_hinting(true)`. Wasmtime defaults it off
(`Tunables::branch_hinting`), so before that line every hint in every
module was parsed by nothing. It is on for every compile, hinted module or
not: a module without the section is unaffected, and the flag is not part
of the `cwasm` compatibility check.

### The loop

```bash
# 1. count what a real run does, on Helios, one boot per workload
tools/wasi-apps/collect-branch-profiles.py --arch x86-64 --accel kvm

# 2. rebuild; build.sh writes the recorded hints back in
tools/wasi-apps/build.sh
```

**Instrument.** `helios-branch-hints instrument` rewrites a module so every
`if` and `br_if` bumps one of a taken/not-taken pair of 64-bit counters,
and writes a site map from counter index back to the *original* module's
`(function, offset)` — the pair the proposal addresses a hint by, counted
from the start of the function body, which is what
`FuncEnvironment::take_branch_hint` subtracts. The rewrite splices bytes:
no function is renumbered, and every section the tool has no opinion about
survives byte for byte.

Three decisions the rewrite makes, and why:

| Decision | Why |
| --- | --- |
| The counters live in the module's own memory 0, in pages carved out by raising the memory's *minimum* size | A wasi host function reads iovecs from the default memory and nowhere else, so the dump has to write through memory 0; the kernel's pooling allocator gives an instance one memory; a global per site would put hundreds of kilobytes into the instance's vmctx and past `max_core_instance_size`. wasi-libc takes every heap byte from `memory.grow`, which starts above the minimum, so the reserved pages are never handed to the program. |
| The probe is `local.set`, one call, `local.get`, and the counter address is computed with arithmetic rather than a branch | The probe must not itself change the branch behaviour being measured, and a branchless address keeps the instrumented module's own layout out of the counts. |
| Both of a program's exits are covered: the `_start` export is repointed at a wrapper that dumps after it returns, and every `call` to the `proc_exit` import is rerouted through a wrapper that dumps first | wasi-libc's `_start` returns on success and calls `proc_exit` on failure, and a program that calls `exit()` from inside `main` never returns at all. Missing either exit loses the whole profile. |

**Record.** The counters come back on the program's own stdout, framed by
`!helios-branch-profile-1` and `!helios-branch-profile-end` markers so the
surrounding console traffic is not mistaken for counts and a guest that
died mid-dump is rejected rather than half-read. That channel already
exists: `helios-inspector vm … shell -c` runs a command in the guest and
brings its output back. The LLVM raw-profile export of (a) was the
alternative and does not fit: it carries the *kernel's*
`-C profile-generate` counters out of a kernel built for it, and has no way
to describe a counter array belonging to a user-mode wasm instance, so a
branch profile would need a second export and a second instrumented build.

`helios-branch-hints record` sums one or more captured runs against the
site map into a profile under `tools/wasi-apps/branch-profiles/`. A
profile has to be committed for a rebuilt artifact to keep its hints — CI
rebuilds the artifacts from `build.sh` on a cache miss and `artifacts/` is
not in the repository — and **none is committed today**, for the reason
under "What it is worth" below. Sites executed fewer than
`MIN_OBSERVATIONS` times are dropped when a profile is written: they can
never produce a hint, and keeping them makes the file an order of
magnitude larger for no decision.

**Hint.** `helios-branch-hints hint` writes the section into the original,
uninstrumented module, immediately before the code section, and
`tools/wasi-apps/build.sh` calls it for every staged artifact a profile
exists for, naming the artifact and the profile in the build log. Two
constants decide what gets a hint, both in `tools/branch-hints/src/lib.rs`
with their reasoning:

| Constant | Value | Why |
| --- | --- | --- |
| `MIN_OBSERVATIONS` | 1000 | A hint moves the unlikely successor out of line, so one taken from three executions can cost every later execution an extra jump for a bias that was never measured. A thousand executions puts the binomial 95% interval of an observed 90/10 split inside ±2 points. |
| `HINT_RATIO` | 0.90 | Cranelift's whole response is layout. At 90% the cost is bounded by one extra jump on a tenth of the executions, against contiguous layout on the other nine tenths. Cranelift has no way to express "slightly", so a merely-probable hint is worse than none. |

A profile is only applicable to the build it was recorded from, and two
checks say so before a byte is written: the sha256 of the code section
(the bytes a recorded offset indexes into, and nothing else, so a rebuild
that only reorders custom sections still matches), and, per site, that the
module really has an `if` or a `br_if` at that offset. Either failing
fails the build, because a hint written from a stale offset is a silently
wrong compilation.

### What it is worth

The hints change block layout and nothing else, so the measurement is the
compute-parity workloads on the `x86-64-kvm` bench lane (§3.6), hinted
artifacts against unhinted ones. Two `bench-suite` dispatches on the same
host CPU model, both advisory on the shared runner: unhinted
[34005864567](https://github.com/lexoliu/helios/actions/runs/34005864567)
against hinted
[34005865823](https://github.com/lexoliu/helios/actions/runs/34005865823),
AMD EPYC 7763, 4 vCPUs, KVM.

The hinted side carried the profiles the loop recorded on Helios:
`qjs.wasm`, 22,313 branch sites, 54 above the observation floor, 36
hinted; `python3.wasm`, 126,615 sites, 2,404 above the floor, 1,777
hinted, from `cpython-json` and `cpython-regex` summed. Helios warm
medians in ms, with the bootstrap 95% interval of the median:

| Workload | branch hints | unhinted ms | hinted ms | ratio | verdict |
| --- | ---: | ---: | ---: | ---: | --- |
| `quickjs-loop` | 36 | 45 [42, 48] | 40 [39, 41] | 0.889x | within noise |
| `cpython-json` | 1,777 | 325 [322, 331] | 312 [309, 317] | 0.958x | within noise |
| `cpython-regex` | 1,777 | 467 [466, 472] | 450 [446, 456] | 0.965x | within noise |
| `aot-curl` | 0 | 458 [436, 464] | 436 [431, 456] | 0.954x | within noise |
| `wasm-simd-lanes` | 0 | 13 [10, 13] | 13 [12.5, 13.5] | 1.000x | within noise |

`cpython-json` and `cpython-regex` run the same hinted module and so share
its hint count.

Noise floor 14.1%, from the control workload before and after the suite.

The two CPython rows have disjoint intervals and moved 3.5% and 4.2%. So
did `aot-curl`, by 4.6% — and `aot-curl` carries no hints at all, because
it times the compiler rather than the compiled code. A change that moved
the hinted workloads and the unhinted control by the same few percent
moved the machine, not the layout: on this lane the effect of the hints is
smaller than what separates two runs of the same code.

So the tooling is what lands. The artifacts ship **unhinted**: no profile
is committed, `build.sh` finds none, and every artifact is byte for byte
what it was. Re-recording one is a single command, and what would make the
question answerable is a lane that can resolve a few percent — the
dedicated runner of docs/benchmarks.md — rather than a different producer.

The paired mode `--baseline-ref` added in #178 is not the instrument for
this: it shares `artifacts/` and the `helios-cli` that compiles them
between the two columns and varies only the kernel image, so both columns
would carry the same hinted wasm. Pairing an artifact-level change needs
the baseline checkout to stage its own artifacts, which it deliberately
does not.

## Issues filed

- #70: in-kernel `-C profile-generate` runtime with `-Z no-profiler-runtime`,
  the linker-script sections, the raw-profile writer and the export call.
  Implemented; described above.
- #71: branch-hint feedback for the compiler plugin: instrumenting the
  suite's wasm inputs, writing `metadata.code.branch_hint`, and re-hinting
  in `build.sh`. The producer is implemented and described above; the
  artifacts ship unhinted, because the effect is below what the lane can
  resolve.
- #211: `-C profile-use` for the kernel, the `profile-use` build kind and
  the paired `suite-pgo` job. Implemented; described above.
- #327: the two columns of that job built into one directory once a
  release kernel became a `profile-use` build itself, so both booted one
  image. A profile named on the command line now keys a target directory
  of its own; described under "Where each build lands".
