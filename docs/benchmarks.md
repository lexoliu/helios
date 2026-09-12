# Benchmarks

Helios is designed around wasm end to end, so the claims worth making are
about what that buys: how fast an instance starts, what a host call costs
against a syscall, how fast two programs talk, what a context switch
costs, and whether the in-kernel network and file paths keep up with
Linux. This page states exactly what is compared, how the numbers are
taken, and how to reproduce any of them from a tag.

The tooling lives in `tools/bench/` (`helios-bench`, a `uv` project); the
workload definitions in `tools/wasi-apps/workloads.json`; the CI workflow
in `.github/workflows/bench-suite.yml`. Nothing on this page is measured
on a developer machine.

## What is compared

Three sides, on one machine, in one workflow run:

| Side | What runs | Where |
| --- | --- | --- |
| Helios | the kernel under QEMU, programs as signed `cwasm` | `helios-inspector vm … workload-bench` |
| Linux + Wasmtime | the same wasm, precompiled by `wasmtime compile` from the same Wasmtime release Helios vendors, run by `wasmtime run --allow-precompiled` inside a Fedora guest | `tools/wasi-apps/fedora_qemu_baseline.py` |
| Native Linux | a C or distribution-native equivalent inside the same Fedora guest | same guest, `tools/bench/native/*.c` |

A fourth side appears when the run is **paired**: `Helios (baseline)`, a
second Helios image built from another commit and timed against the first
on the same host, in the same job. It is not a fourth runtime — it is the
same kernel at another revision, and it exists so that a few-percent
change can be told apart from a change of runner. See "The paired mode"
below.

Every side gets the same QEMU release, accelerator (KVM), vCPU count,
memory, virtio block/net/rng devices,
and network backend. The pins are in `tools/bench/manifest.toml`; the
runner refuses to start on a host that deviates from its lane unless the
run is explicitly advisory, in which case the deviations are written into
the report and the report is marked non-publishable.

The Linux guest is a Fedora Cloud Base image pinned by SHA256 in
`tools/wasi-apps/fedora_qemu_baseline.py`; the Wasmtime release it runs is
pinned there too and must match the vendored tree in
`.github/actions/checkout-wasmtime/action.yml` (the report records both).
Helios cannot load the Linux `cwasm` and Linux cannot load the Helios
one: Cranelift emits for `aarch64-unknown-none` on one side and
`aarch64-unknown-linux` on the other, so "same AOT artifact" means the
same wasm input, the same compiler revision, and the same optimisation
level, with both artifacts' SHA256 recorded in the report.

## Workloads

Each workload isolates one design claim; its class names that claim, and
its `counterparts` in `workloads.json` say what the Linux sides run. A
`null` counterpart is reported as uncovered, never approximated, and the
workload's `uncompared` entry records why; the renderer prints that
reason under the table the cell is missing from.

| Class | Workload | Helios | Linux + Wasmtime | Native Linux |
| --- | --- | --- | --- | --- |
| startup | `instance-startup-{1,100,500}` | `procbench startup N hello hold` through `helios:system/programs`; time to first stdout byte per instance, memory per instance as the drop in `helios:system/stats` available memory while all are alive | `procbench` spawning `wasmtime run --allow-precompiled hello.cwasm hold` | `procbench` spawning the C `hello`; RSS from `/proc` |
| startup | `spawn-wait` | 200 sequential spawn+wait | same, Wasmtime child | same, native child |
| startup | `process-startup` | 20 × `dash -c true` | 20 × `wasmtime run hello.cwasm` | same |
| hostcall | `hostcall-loop` | 2 000 000 × `wasi:clocks/monotonic-clock.now` | same wasm | 2 000 000 × `clock_gettime(CLOCK_MONOTONIC)` |
| ipc | `pipe-pingpong` | 20 000 × 64-byte round trip through a child's stdin/stdout | Wasmtime `pipe-echo` child | C `pipe-echo` child |
| ipc | `pipe-stream` | 64 MiB through the child | same | same |
| ipc | `stdio-pipe` | coreutils pipeline | the same coreutils module with its WASIX imports stubbed (`coreutils-wasi.wasm`) under `wasmtime run --dir` | same |
| sched | `sched-tasks` | 64 cooperative tasks × 2000 `yield_now` (one host call each) | uncompared (Wasmtime's CLI has no cooperative scheduler for a CLI program) | 64 threads × 2000 `sched_yield` |
| net | `tcp-throughput`, `tcp-upload`, `wasi-tcp-throughput`, `wasix-tcp-throughput` | 64 MiB streams through the in-kernel stack | `wasi-tcp-throughput.wasm` labelled per row under `wasmtime run -S inherit-network` | Python client |
| net | `curl-local-http`, `curl-http-throughput` | `curl.wasm` over `wasi:http` | `wasi-curl.wasm` (the same curl CLI contract over plain WASI sockets) under `wasmtime run -S inherit-network` | curl |
| net | `tcp-latency` | 5000 × 16-byte round trip to a host echo server | same wasm | C client with `TCP_NODELAY` |
| fs | `fs-smallfiles`, `fs-readstream` | coreutils on the embedded filesystem root | `coreutils-wasi.wasm` under `wasmtime run --dir` | ext4 in the guest |
| compute | `quickjs-loop`, `cpython-json`, `cpython-regex`, `wasm-simd-lanes` | interpreter or SIMD loops | same wasm | native QuickJS/CPython/NEON-or-SSE probe |
| compute | `aot-curl` | compiler plugin AOT of `curl.wasm` | `wasmtime compile` of the same input | uncompared (`wasmtime compile` is the native equivalent of the in-guest step) |

`headline: true` marks the rows the README table and the regression gate
carry. Every compared row is a parity check, not a claim: Helios running
the same wasm on the same Cranelift must be within noise of Linux +
Wasmtime, whatever the workload's class, and a significant loss on any
of them is flagged `parity_bug` in the report and filed as a bug rather
than reported as a number.

Workloads print secondary measurements as `bench.<name>=<value>` lines
(latency percentiles, bytes per instance, switches per second); both
harness sides collect them into the report so they can be compared
without either side knowing about the other.

Known limits recorded by the suite itself:

- The pooling allocator caps live component instances at Wasmtime's
  default of 1000, system programs included, so the density set stops at
  500 instances until that limit is a kernel decision.
- `fs-*` runs on the embedded filesystem root, not on a block device;
  a virtio-blk-backed filesystem (issue #15) is what makes the file I/O
  class comparable to ext4.
- The network class needs a multi-queue `tap` with vhost-net to mean
  anything: slirp is single-queue with no offload, so a run taken on one
  measures neither the driver's multiqueue path nor its checksum and TSO
  paths (docs/networking.md).

## Statistics

Per cell (workload × side), `iterations` executions (11 by default):

- The first `warmup_discard` (1) is the **cold** series and is reported
  separately; the remaining ten are the **warm** series every headline
  number and the gate use.
- Per series: median, quartiles and IQR, mean, standard deviation,
  coefficient of variation, min and max, and a percentile-bootstrap 95%
  interval of the median from 10 000 resamples drawn with a fixed seed
  (`bootstrap_seed` in `manifest.toml`), so the interval in a report can
  be recomputed from its raw iterations.
- A warm series whose CV exceeds `cv_bound` (0.15) is **rejected**: it is
  printed with a marker, excluded from comparisons and from the gate.
- A workload a side could not measure at all is a **failed** cell, not a
  missing one: every harness runs with `--keep-going`, records the
  failure and its reason, and goes on to the next workload, so a report
  accounts for every cell of the matrix. A failed cell takes part in no
  comparison and the reason is printed under the class table.
- A workload that takes the guest down with it costs its own class and no
  other. The Helios side boots one guest per workload class, so a kernel
  panic ends that class: the inspector's frame reader recognises the
  kernel's panic line on the console it shares with the RPC frames and
  fails the call in flight instead of waiting for a reply that a dead
  kernel will never send, the workloads that class never reached are
  written out as failed cells naming the panic, and the next class starts
  a fresh guest. The lane still fails — a report with a panic in it is
  not publishable — but it fails with every cell it could still measure.
- Machine noise is measured, not assumed: the `control_workload`
  (`quickjs-loop`) runs before and after the suite on every side, and the
  **noise floor** is the larger of the control's median drift and its CV.
  Every side times an iteration in floating-point milliseconds — the
  inspector from an `Instant`, the Linux runner from
  `time.perf_counter_ns()` — so the floor reports how far the machine
  actually moved. Truncating to whole milliseconds used to put a floor
  under the floor: the control finishes in about thirty of them, so one
  tick was 3.3% before the host had drifted at all (#278).
  A floor above `cv_bound` (0.15, the bound a single row's dispersion is
  held to) makes the whole comparison **inconclusive**: the host moved
  by more than any effect a change could show, so no row gets a verdict.
  The paired gate fails the check and asks for a rerun; the cross-run and
  profile-use tables carry the banner and block nothing (#292).
- A cell whose warm CV is past `cv_bound` is **rejected**: its median
  cannot be trusted to detect a regression, and a headline workload without
  a trustworthy pair blocks as incomplete evidence. Before the gate reads
  the report, the runner times every headline workload with a rejected
  Helios or baseline cell again, once, on every Helios image the run has,
  through the same driver invocation into `retake/` beside the first pass,
  and the retaken cells replace the first attempt on both images. The run
  record names them and the gate table says so (#295).
- A headline regression has to show twice. When the paired gate would
  block on a workload, the runner times that workload again on both
  images back to back into `reconfirm/`, and the second pair replaces the
  first: a regression that is the change's own reproduces, a drift
  between two boots does not. One pass per run, so a host that drifts
  twice in a row still fails the check; the run record names the
  reconfirmed workloads and the gate table says so (#297).
- A comparison between Helios and a Linux side is **significant** when
  the two warm bootstrap intervals do not overlap and the ratio of medians
  moves by more than the noise floor; otherwise it prints "within noise".
- The regression gate makes the same call twice, against two baselines,
  and the difference between them is which one can be trusted to block.
  A headline workload is regressed when the two warm bootstrap intervals
  are disjoint and the median moved by more than the noise floor.
- The gate judges every measurement a cell carries, one row each: the
  host-side `elapsed_ms` of the round trip, and each `bench.<name>`
  metric the workload printed from inside the guest. A workload times
  part of itself separately because the round trip averages that part
  away — `procbench` times the teardown of a startup batch on its own,
  for the kernel allocator — and a gate that compared the round trip
  alone would put it straight back (#279).
  - The metric's name carries its unit, because `bench.<name>=<number>`
    on stdout is all either harness gets, and the unit is what says how
    to read a shift. `_us`, `_ms`, `_ns`, `_per_call` and `_per_op` are
    durations and `_per_s`, `_per_second` are rates of them, so a
    positive shift is a regression for the first group and an
    improvement for the second; `_bytes` is a footprint. A metric whose
    name ends in no known unit stops the gate by name rather than being
    guessed at.
  - The noise floor is the control workload's drift, so it bounds how
    far the *machine* moved and applies to durations and rates. A
    footprint is exposed to the page rather than to the clock: memory is
    handed out in pages and `memory_per_instance_bytes` is a delta of
    available bytes over an instance count, so a shift under 4 KiB is
    accounting and anything past it counts, with no timing floor above
    it that a real regression could hide under.
  - A row the gate cannot attribute to the change is printed as
    `diagnostic` and blocks nothing. A workload's latency metrics are
    computed over the samples of *one iteration*, so a maximum is the
    tail of that boot's scheduling order — no sample size makes it
    attributable — and a percentile is only a percentile when enough
    samples lie past its rank: `LatencySamples::report` prints
    `<prefix>_samples` for exactly this, and the gate wants ten samples
    beyond the rank, so p99 needs a thousand and p99.9 needs ten
    thousand. Over a hundred samples the nearest-rank p99 *is* the
    second largest, and a paired run of two identical kernels moved that
    number 14% (#286). A percentile whose count the report does not
    carry is diagnostic rather than trusted.
  - A metric row whose warm coefficient of variation exceeds the run's
    `cv_bound` on either side is printed with that reason and takes part
    in no verdict, the way a variance-rejected cell does.
  - A metric only one column measured is named under the table and
    blocks nothing: the run that introduces a metric has nothing to
    compare it against.
  - **Paired**, against the `Helios (baseline)` side of the candidate's
    own report: one host, one job, the two images booted back to back for
    every workload. Nothing about the machine differs between the
    columns, so this half blocks on a headline regression whether or not
    the report is publishable. Every headline also needs a valid cell
    from both images: a missing, failed or variance-rejected cell blocks
    as incomplete paired evidence, rather than disappearing from the
    acceptance decision. Comparable rows remain in the report.
  - **Cross-run**, against the newest `dev` report of the same lane:
    another job, another runner. This half blocks only when both reports
    are publishable *and* their run records name the same host CPU;
    otherwise it comments the table and enforces nothing.
  The gate comment on a pull request prints the paired table first.

## Reading a launch's phases

`workload-bench` times a launch end to end; when that number needs a
breakdown the kernel can say where the launch itself went. A launch —
an `exec`/`spawn` host call, or a guest's own `proc_spawn*`/`proc_exec*`
syscall — records the kernel monotonic timestamp of every phase
boundary it crosses into a fixed-capacity in-memory timeline and emits
one `DEBUG` event under the target `helios_kernel::exec::phases` when
the launch ends: at completion, or at the failure exit with whichever
phases it reached. The line's `*_ns` fields are each phase's offset in
nanoseconds from `rpc-arrival`:

| field | phase boundary |
| --- | --- |
| `rpc_arrival_ns` | the launch call entered the kernel; always `0`, the epoch |
| `source_read_ns` | the program's bytes are read out of their source (`source_bytes` carries the size) |
| `trust_ns` | artifact trust established — the bootfs trailer parse, the signature check for a signed artifact, or — on a raw-wasm source — the whole in-kernel compile+sign between `load_begin` and here |
| `cache_lookup_ns` | the deserialize cache answered (`cache_hit` carries its answer) |
| `deserialize_ns` | the `cwasm` payload deserialized; present only on a cache miss |
| `instantiate_pre_ns` | the `InstancePre` cache answered or `instantiate_pre` built (`instantiate_pre_hit`) |
| `load_begin_ns` / `load_complete_ns` | `load_executable` entered / returned |
| `task_begin_ns` | the run task is live |
| `shared_memory_ns` | a core module's shared memory is prepared |
| `store_prepare_ns` | the store and its filesystem snapshot are prepared |
| `instantiate_ns` | `instantiate_async` returned — memory slot, data segments, imports resolved |
| `start_ns` | the run function resolved and guest start is dispatching |
| `guest_begin_ns` / `guest_end_ns` | guest code running / returned |
| `store_teardown_ns` | a core module's store is torn down |
| `completion_ns` | the run task finished |
| `reply_ns` | the launch call's reply — or the syscall's result — is being written |

A phase the launch never reached — the warm launch's `deserialize`, a
failed launch's tail — leaves no field, and `phase_count` says how many
the line carries. The phase a launch's duration spent in is the gap
between one field and the next; the guest's own run is
`guest_begin_ns` to `guest_end_ns`.

The line emits once, after the guest ran, so the serial write — one
UART MMIO exit per byte — never lands inside a measured interval. That
is the whole reason for the single-event shape: an event per boundary
would price each offset at the ~150-byte serial write it costs.

`op` names the launch call (`exec`/`spawn`). `program`'s provenance
differs per surface: an RPC launch carries the request's `path`; a
guest `proc_spawn*` carries `argv[0]`; a guest `proc_exec*` carries
the decoded `name` operand — which for a PATH-resolving caller like
dash is the resolved path, so `program=/bin/python3` there and
`program=dash`-style argv names on the spawn surface. `instance` is
the id the registry assigned the launch — `0` when it failed before
registering, and the real id even on a trapped guest, since the run
task sets it before the guest ran. `end` says which exit the launch
took — `completed`, `refused`, `failed` — with `error_kind` (a
`ProgramExecErrorKind` name) or `errno` when the exit carried one.
The calling task writes `rpc_arrival` through `load_complete` and
`reply`; the run task writes `task_begin` through `completion`, through
the same timeline the launch handed it. `reply` is last only on a
buffered `exec` — on `spawn` and `proc_exec*` the reply boundary is
recorded while the run task is still starting, so its offset sits
before `task_begin_ns`.

The target is off by default, so a boot that never asked for it reads
no timestamp and keeps no timeline — the whole cost is one `enabled`
check at `rpc-arrival`. Open it for one session with `--enable-target`,
either on `vm` (before the session action runs) or on `tracing`
(before the stream starts):

```bash
helios-inspector vm --arch x86-64 --release --accel kvm \
    --boot-program dash --boot-program debugger --boot-program python3 \
    --no-compiler-plugin \
    --enable-target helios_kernel::exec::phases \
    shell -c 'python3 -c "print(1)"; python3 -c "print(2)"'
```

The lines land on the debug serial line, so `<runtime>/debug-serial.log`
(docs/debug-serial.md) holds them — it carries console escape bytes, so
grep it with `-a`. The same target streamed live is `helios-inspector
tracing --enable-target helios_kernel::exec::phases --min-level debug
--target-prefix helios_kernel::exec::phases`.

Two launches of the same program are the cold/warm pair the launch-cost
question is usually about: the cold line reads `cache_hit=false` and
carries `deserialize_ns`; the warm line reads `cache_hit=true` and does
not. The `smoke-x86-64` step "Run CPython twice and record its launch
phases" runs exactly this under KVM, so a PR's x86-64 launch-phase split
is read from that step's `debug-serial.log` artifact rather than
reproduced locally.

## Reports and where the numbers come from

One `report.json` per lane per run (schema in
`tools/bench/src/helios_bench/report.py`): hardware, every pin including
the SHA256 of every wasm and `cwasm` the run used, every iteration of
every cell, the statistics above, the comparisons and verdicts, and the
run's GitHub id. It is uploaded as the `bench-report-<lane>` workflow
artifact, and on a tag it is attached to the release so a paper can cite
the tag.

Every number in this repository's documentation is traceable to one run
id: `helios-bench render readme --run <id>` and `render docs --run <id>`
only render from reports committed under `docs/benchmarks/runs/<id>/`
(fetched with `helios-bench fetch --run <id>`), they refuse a report whose
own run id differs, and the `tooling` job of the workflow re-renders the
committed sections and fails when the text no longer matches the report.

## Cells that are known to fail

A red cell in a published table is either a bug of ours with an issue
number or a limit of the runner, and the table says which. The report's
`failures` map carries the harness's own reason for every one of them, so
the table is generated from what the run actually saw rather than from
this page.

These are the cells that failed in run 33959252438, the run the results
below are rendered from, and why:

| Cell | Sides | Why |
| --- | --- | --- |
| `instance-startup-100`, `instance-startup-500` | Helios | The kernel heap was a fixed quarter of the guest and an instance costs it ~8.1 MiB, so the 46th spawn was refused with a typed `SpawnErrorKind::OutOfMemory` while the user pool was untouched (#130, fixed since; see `docs/memory.md`). The next ceiling was the executor's fixed 768 KiB instance task share at about ninety instances (#159, #142, fixed since: the arena is a share of the machine and its bytes are fungible across block classes). `instance-startup-100` gates the lane again; `instance-startup-500` does not, because 500 instances want about 6.1 GiB of guest and the lane's has 2 GiB. `instance-startup-500`'s Linux half is skipped by name, because a cell whose Helios half cannot be measured has nothing to compare against; `instance-startup-100` runs on both sides. |
| `tcp-throughput`, `wasi-tcp-throughput`, `wasix-tcp-throughput` | Helios | The guest receive path stops answering and the workload's own deadline fails it (#143). |
| `curl-http-throughput` | Helios | Never reached: the `net` class spent its share of the Helios side's budget on the three cells above and was killed at 589 s, so this one is recorded as unmeasured rather than left out. |
| `tcp-latency` | all three | The driver bound the host echo server to 127.0.0.1 while every side reaches the host at the lane's `net_host` (10.77.0.1 on this lane), so no side could connect and all three exited non-zero (#150, fixed since). |

The refusal behind #130 was correct for the pool it was asked about and
wrong about which pool had run out. `docs/memory.md` states the
relationship between guest memory and the two domains that replaced it:
all usable memory is user pool and the kernel heap draws on it, so the
instance ceiling is a property of the guest's memory. With that fixed the
density workload reached instance 104 and was refused by the executor's
fixed 768 KiB instance task share instead (#159). That arena is sized
from the boot memory plan now — 7.25 MiB per processor on this lane's
2 GiB guest — and buddy-split, so freed bytes serve any block class
(#142). `instance-startup-100` is back in the gating set;
`instance-startup-500` stays out against the guest's memory rather than
against the executor.

The per-processor task arena that #132 records — where the density cells'
refused spawns cost every later spawn in the same guest — did not recur
in this run: `spawn-wait` and `process-startup` were both measured after
them.

### Nothing in the run is unbounded

A guest that stops answering is the failure mode this suite meets most
often, and it is bounded three times over, because each bound catches
what the one below it cannot see.

- **Per iteration.** Every workload iteration runs under
  `--workload-timeout-seconds`; the iteration that elapses is a failed
  cell naming the workload and the iteration.
- **Per guest step.** The steps around the workloads — the profiler
  hand-off, the profile and metric reads, the tracing fetch — talk to the
  same guest, so they run under the same deadline. Without that, a run
  whose workloads were all recorded still sat on a dead VM: run
  33952047436 hung there for ninety-five minutes with QEMU alive behind
  it, until CI cancelled the job.
- **Per side.** `--helios-side-timeout-seconds` bounds the whole Helios
  side, and every boot inside it shares it out as the run goes: one boot
  per class (or, paired, one per workload per image), plus the control
  workload before and after for each image. Nothing is reserved off the
  front — the builds happen before the budget starts, and the control
  boots are counted among the boots rather than set aside at the
  per-boot cap, which is what made run 33997256902 refuse all
  forty-eight of its boots as over budget without booting once. Each
  boot may take at most its share of what is left, so a boot that
  finishes early widens the share of every boot behind it and the ones
  at the end are protected by the same arithmetic as the ones at the
  start. A boot that never reaches the debugger answers no deadline at
  all, so this is what keeps a wedged boot costing one boot rather than
  the lane, and a side that measured nothing fails the run naming why
  its first boot was refused. The Linux side has had the same bound as
  `--side-timeout-seconds` since it lost a side to one hung workload.

The bugs the first runs of this suite found are fixed: the x86 kernel
refusing a multi-queue `vhost` tap (#91), a user-mode spawn storm
panicking the kernel through the task arena (#94), the OOM killer
condemning a fresh victim per grow attempt (#100) and then panicking on
an already-inactive instance (#114), the guest receive path stalling
around 300 KB (#93), and two guests sharing one runtime directory (#98).
A cell that fails now is news, and belongs in a new issue quoted from the
report.

## One lane, and why

The suite measures **x86-64 Linux under KVM** and nothing else.

Nearly everything it measures — the executor, the component host, the
host-call path, the in-kernel network stack, the block and pipe paths —
lives in the architecture-neutral `kernel/` crate, so a second
architecture repeats the same code through a different backend rather
than covering new ground. What is genuinely per-architecture (trap entry,
IRQ delivery, MMIO, SMP bring-up) is covered by the smoke lanes, which
boot every backend on every change.

There is also no hosted runner that could carry a second lane. GitHub's
Arm runners expose no `/dev/kvm` (probe run 33944339758) and no readable
`/dev/vhost-net`, so an Arm lane there would be an interpreter measuring
an interpreter behind a userspace tap: a different machine, not a noisier
one, which changes what is fast relative to what — the one thing a
benchmark exists to measure.

An aarch64 number therefore comes from a dedicated Apple Silicon machine
or a developer's own arm64 box, taken by hand with the same harness
(`helios-bench run --lane …` against a lane added to `manifest.toml` for
that machine), never from hosted CI. AGENTS.md §3.5's arm64 baseline is
that kind of measurement.

## Dedicated runners

Publishable numbers come from a self-hosted machine registered with this
label:

| Lane | Label | Host | Accelerator | Advisory stand-in |
| --- | --- | --- | --- | --- |
| `x86-64-kvm` | `helios-bench-x86-kvm` | x86-64 Linux, `/dev/kvm` | KVM | `ubuntu-24.04` |

Requirements the manifest's host check cannot verify and the machine's
owner must guarantee: the CPU frequency governor is fixed
(`performance`), turbo behaviour is the same for every run, nothing else is
scheduled on the machine while the workflow runs, the same QEMU release
is installed as the lane pins, and the network backend is the lane's
(a `tap` device driven by vhost-net, the only host packet path with more
than one queue and any offload — see docs/networking.md).

Until that machine exists, the lane runs on its hosted stand-in in
**advisory** mode: the report carries `publishable: false`, the README
says so, and nothing blocks. The stand-in has the lane's accelerator and
its network backend; what it cannot promise is an idle machine or a fixed
governor. The repository variable `HELIOS_BENCH_DEDICATED=true` turns on
the dedicated-runner runs for pushes to `dev`; tags always use the
dedicated label.

Every difference between the host a run is taken on and the lane it
claims is recorded by `host-check` and listed in the report's
`deviations`, and any deviation at all makes the report
`publishable: false`.

## The paired mode

A shared runner does not pin the CPU model. Run 33990628290 reported
every workload 20-40% faster than the `dev` run it was compared against
(33987950977), including `quickjs-loop`, `cpython-json` and
`wasm-simd-lanes`, which the change under test — cache-line padding of
three kernel structures — cannot touch. Two consecutive `dev` runs agree
within a few percent. The pull request's run had landed on a faster
machine, and nothing in the comparison could see that (#173).

The paired mode answers it by taking the second column on the same
machine, with the same harness:

```bash
uv run helios-bench run --lane x86-64-kvm --out-dir … --baseline-ref
```

Given a ref, `--baseline-ref` resolves it; given none, it means the merge
base with `dev`, the commit the branch is a change to. The suite checks
that commit out as a git worktree beside the candidate checkout,
`<candidate dir>-baseline-<sha12>`, and times its guest against the
candidate's. The checkout is the candidate's sibling so that the kernel's
`../wasmtime/crates/wasmtime` path dependency resolves to the same
absolute directory for both images: cargo hashes a path dependency
outside the workspace by its absolute path, and a baseline that reached
the vendored checkout through a link of its own compiled every Wasmtime
crate under another crate hash, so the kernel profile matched none of
the symbols named through Wasmtime and the pair timed a profile-guided
candidate against an unprofiled baseline (#359). The baseline's warm
build directory is `target/perf-baselines/worktrees/<sha>/target` under
the candidate's `target/`, where the runner cache restores it; the
checkout reaches it through its `target/` link.

The second image is built the way a release build of the lane is, which
on x86-64 reads the fetched kernel profile (`docs/pgo.md`).
`--baseline-kernel-build release` builds it without one instead: alone,
that pairs this commit's profile-guided kernel against its plain one,
the control of a PGO measurement; with `--baseline-ref`, that commit's
plain kernel. The dispatch input `baseline_kernel_build` is the same
switch in CI.

The baseline checkout supplies a guest and the tooling that guest is
built and booted by; the scheduling harness stays the candidate's. One
harness times both images — the candidate's
`tools/wasi-apps/workload-bench.sh`, its workload manifest, its iteration
counts and budgets — and an image is selected with
`HELIOS_WORKSPACE_ROOT`, the checkout the inspector resolves the guest
against — the kernel artifact, the prebuild manifest, the bootfs sources
and the program manifests. But the `helios-inspector` and `helios-cli` a
side runs are compiled from that side's own ref into its own
`target/release`: the inspector and the guest's debugger speak
`helios-inspector-protocol`, and when a record changed between the two
refs the candidate's decoder asked the baseline guest for a field it did
not send — the baseline's readiness probe failed
`DeserializeUnexpectedEnd`, and the whole pair was lost (run
34551261487, #356). A baseline whose own tooling does not build fails the
run naming its checkout; there is no fallback to the candidate's
binaries, and a run that cannot produce them is refused before the first
boot.

| | Shared by both columns | Per side |
| --- | --- | --- |
| Host | CPU, load, thermal state, QEMU release, accelerator, vCPUs, memory, network backend and its host servers | — |
| Harness | `workload-bench.sh`, the workload manifest, the run's iteration count and budgets | — |
| Tooling | — | `helios-inspector` and `helios-cli`, built from each side's own commit under its own `target/release` |
| Guest inputs | everything `tools/wasi-apps/build.sh` stages under `artifacts/`, linked into the baseline worktree entry by entry; the vendored Wasmtime checkout, `../wasmtime` of both checkouts at the one absolute path (#359) | — |
| Guest | — | the kernel image, the bootfs it carries, the compiler plugin, the guest programs the prebuild signs |

Two checkouts that turn out to be one build are refused before the first
boot: the suite asks the inspector for each image's guest artifact
(`helios-inspector vm --arch … kernel-path`, so the mapping from
architecture and profile to Cargo target and artifact name has one
definition), digests both, and fails the run when they match. There is
nothing for a comparison between one build and itself to say.

Two guest images cannot share a guest, so the boot is the smallest unit
the pairing has. A paired run therefore boots **one guest per workload
per image** rather than one per class, and the two boots of a workload
are adjacent: baseline, candidate, next workload, and which of the two
goes first alternates so that neither systematically holds the earlier
slot. Within a boot the iterations are what they always were — iteration
1 cold, the rest the warm series — because a guest per iteration would
make every iteration a cold one and leave nothing for the CV bound, the
bootstrap interval or the cross-run comparison to stand on.

What the two images share, and therefore cannot explain a difference
between them: the host and its CPU model, load and thermal state; the
QEMU release, the accelerator, the vCPUs and the memory; the network
backend and the host HTTP, TCP and echo servers on it; the workload
manifest, read from the candidate checkout for both; everything under
`artifacts/` that `tools/wasi-apps/build.sh` stages, linked into the
baseline worktree entry by entry rather than copied; and the vendored
Wasmtime checkout, which both checkouts reach as `../wasmtime` at the
one absolute path, so both kernels compile against one revision under
the same cargo crate hashes (#359). What differs is the kernel image, the
bootfs it carries (the compiler plugin included) and the
`helios-inspector`/`helios-cli` that build and boot it — each side's own,
compiled from the side's own commit.

The report carries the second column as the `helios_baseline` side, and
its run record carries both commits (`helios_git_sha` for the candidate,
`baseline_git_sha` for the baseline) and both tooling revisions
(`inspector_git_sha` and `baseline_inspector_git_sha`), which the
rendered tables print beside the kernels'. A run that was asked to pair
and could not build or measure its baseline is a failed run, not a
report with one column missing.

A boot's unix sockets do not live in its runtime directory. That path is
the caller's, and the paired layout nests it per image and per workload,
which took it past `sockaddr_un::sun_path` and made QEMU refuse the
monitor socket with both guest images already built. The inspector puts
`debug.sock`, `monitor.sock` and `qmp.sock` in a short directory of its
own under `$XDG_RUNTIME_DIR` and links it as `<runtime>/sockets`; see
`docs/debug-serial.md`.

`bench-suite.yml` runs a pull request in paired mode against
`github.event.pull_request.base.sha`, and `workflow_dispatch` takes a
`baseline_ref` input. An advisory dispatch with an explicit baseline is
revision acceptance: it runs only `helios` and `helios_baseline`, with
all workloads, warm iterations, before/after compute controls, and the
same enforced paired gate. It does not run the unrelated Linux
comparisons or independent profile-generation/PGO experiment. Unpaired
dispatches, labelled PR runs, dedicated runs and publication events retain
the full suite and profiling flow. Mode selection is computed once by the
tooling job, so profile generation starts only after tooling succeeds.

Each guest boot gets fresh host HTTP, TCP throughput, and echo listeners
with the same payloads and configuration, provisioned before timing and
closed after that guest finishes. Reusing a listener across rebooted
images also reuses the peer's old connection state: the later guest can
pay a SYN retransmission for every reused four-tuple, contaminating even
its warm series. Ordinary measurements isolate these listeners; explicit
`--reuse-host-listeners` is reserved for reconnect diagnosis and marks the
report as unsuitable for performance acceptance.

The `bench-x86-64-linux` lane of `ci.yml` is unchanged except that the
inspector now writes the host CPU model into the `run` record it emits,
so a comparison between two of its runs can tell one machine from two.

For TCP reconnect diagnosis, a dispatch can explicitly set `tcp_probe=true`
and supply `baseline_ref`. That mode explicitly enables
`--reuse-host-listeners` and captures two iterations of `tcp-throughput`
on each image with `--net-queues 1`, as required by QEMU's packet filter.
Setup provisions a matching single-queue TAP, and the explicit capture
selects QEMU's userspace TAP path (`vhost=off`), because
the filter cannot attach to vhost-net. Packet captures and raw logs are
retained in the runtime artifact; the diagnostic report is named
`tcp-probe-<lane>`. It is always
non-publishable and does not run the performance gate or PGO experiment.
It is not acceptance evidence: normal multi-queue acceptance still runs
the full workload set and its configured warm series.

## Reproducing a published number

```bash
git checkout <tag>
cd tools/bench && uv sync
uv run helios-bench host-check --lane x86-64-kvm     # must print no deviation
uv run helios-bench run --lane x86-64-kvm --out-dir ../../target/bench/x86-64-kvm
uv run helios-bench render tables --report ../../target/bench/x86-64-kvm/report.json
```

`run --dry-run` prints the exact harness commands and the host
deviations without running anything, the baseline worktree included: it
resolves the ref but creates nothing. `--sides helios` or
`--sides linux_native,linux_wasmtime` runs one side; `--workload` repeats
to select workloads. The runner drives `tools/wasi-apps/linux-gap-bench.py`
for both sides; every intermediate file (per-class JSONL, the guest's
JSONL, the Fedora provisioning logs) stays under the output directory.

## Reading the plots

`helios-bench render plots` draws one SVG per class and one for the
headline set. Class plots show the median warm wall time per side on a
log axis with the bootstrap interval as error bars: shorter is better,
and two bars whose error bars overlap are not distinguishable. The
headline plot shows Helios's speed-up over each Linux side (other median
over Helios median, log axis): right of the 1x line Helios is faster,
left of it Helios is slower, and bars inside the noise floor mean nothing.

## Results

<!-- helios-bench:begin run=33959252438 -->
Rendered from CI run [33959252438](https://github.com/lexoliu/helios/actions/runs/33959252438) by `helios-bench render docs --run 33959252438`; the reports it was rendered from are committed under `docs/benchmarks/runs/33959252438/`.

### Lane `x86-64-kvm`

Advisory: this report is from `GitHub Actions 1000006547`, not a dedicated runner. Its numbers show the shape of the comparison, not a publishable result.

| Pin | Value |
| --- | --- |
| Helios | `5b9c36c5cf28e2bc51ae3514d23d1b31f2f236c6` |
| Vendored Wasmtime | `6bbaceda21b3de992508f1c26e45f66bfd175e68` |
| Wasmtime on Linux | `wasmtime-v48.0.0-x86_64-linux` |
| Fedora image | `28680fe5b371a5a8…` |
| QEMU | pinned `8.2.2`, ran `8.2.2` |
| Guest | 4 vCPUs, 6G (Helios) / 4G (Linux), `tap` network, virtio-blk-pci, virtio-net-pci, virtio-rng-pci |
| Host | AMD EPYC 7763 64-Core Processor, 4 logical CPUs, kvm |
| Noise floor | +15.1% from `quickjs-loop` before and after the suite |

#### Instance start-up

![Instance start-up on x86-64-kvm](benchmarks/runs/33959252438/x86-64-kvm-startup.svg)

| Workload | Helios | Linux + Wasmtime | Native Linux | vs Wasmtime | vs native |
| --- | ---: | ---: | ---: | ---: | ---: |
| `instance-startup-1` | 16.0 [15.0, 19.0] (rejected) | 6.74 [6.45, 6.83] | 0.76 [0.71, 0.91] | n/a | n/a |
| `instance-startup-100` (headline) | **failed** | n/a | n/a | n/a | n/a |
| `instance-startup-500` | **failed** | n/a | n/a | n/a | n/a |
| `spawn-wait` (headline) | 460 [458, 460] | 1,200 [1,189, 1,207] | 51.6 [50.8, 52.4] | 2.61x | 0.11x |
| `process-startup` | 374 [334, 414] | n/a | 29.6 [29.4, 29.7] | n/a | 0.08x |

#### Host call vs syscall

![Host call vs syscall on x86-64-kvm](benchmarks/runs/33959252438/x86-64-kvm-hostcall.svg)

| Workload | Helios | Linux + Wasmtime | Native Linux | vs Wasmtime | vs native |
| --- | ---: | ---: | ---: | ---: | ---: |
| `hostcall-loop` (headline) | 925 [918, 934] | 482 [481, 483] | 68.3 [68.2, 68.7] | 0.52x | 0.07x |

#### IPC

![IPC on x86-64-kvm](benchmarks/runs/33959252438/x86-64-kvm-ipc.svg)

| Workload | Helios | Linux + Wasmtime | Native Linux | vs Wasmtime | vs native |
| --- | ---: | ---: | ---: | ---: | ---: |
| `pipe-pingpong` (headline) | 432 [429, 434] | 1,185 [1,153, 1,246] | 555 [551, 562] | 2.75x | 1.29x |
| `pipe-stream` | 76.0 [74.0, 76.5] | 130 [128, 132] | 39.5 [37.9, 41.6] | 1.71x | 0.52x |
| `stdio-pipe` | 28.0 [23.0, 28.0] | n/a | 6.76 [6.62, 7.13] | n/a | 0.24x |

#### Scheduling

![Scheduling on x86-64-kvm](benchmarks/runs/33959252438/x86-64-kvm-sched.svg)

| Workload | Helios | Linux + Wasmtime | Native Linux | vs Wasmtime | vs native |
| --- | ---: | ---: | ---: | ---: | ---: |
| `sched-tasks` (headline) | 346 [342, 347] | n/a | 132 [131, 134] | n/a | 0.38x |

#### Network

![Network on x86-64-kvm](benchmarks/runs/33959252438/x86-64-kvm-net.svg)

| Workload | Helios | Linux + Wasmtime | Native Linux | vs Wasmtime | vs native |
| --- | ---: | ---: | ---: | ---: | ---: |
| `curl-local-http` | 14.5 [12.0, 16.0] | n/a | 7.86 [7.76, 8.07] | n/a | 0.54x |
| `tcp-throughput` (headline) | **failed** | n/a | 495 [489, 507] | n/a | n/a |
| `tcp-latency` (headline) | **failed** | **failed** | **failed** | n/a | n/a |
| `tcp-upload` | 1,015 [944, 1,072] | n/a | 573 [547, 696] (rejected) | n/a | n/a |
| `wasix-tcp-throughput` | **failed** | n/a | 496 [490, 503] | n/a | n/a |
| `wasi-tcp-throughput` | **failed** | 499 [492, 504] | 493 [492, 505] | n/a | n/a |
| `curl-http-throughput` | **failed** | n/a | 200 [194, 205] | n/a | n/a |

#### File I/O

![File I/O on x86-64-kvm](benchmarks/runs/33959252438/x86-64-kvm-fs.svg)

| Workload | Helios | Linux + Wasmtime | Native Linux | vs Wasmtime | vs native |
| --- | ---: | ---: | ---: | ---: | ---: |
| `fs-smallfiles` (headline) | 108 [104, 112] | n/a | 101 [100.0, 101] | n/a | 0.93x (within noise) |
| `fs-readstream` | 26.0 [23.5, 29.0] | n/a | 7.73 [7.69, 7.89] | n/a | 0.30x |

#### Compute parity

![Compute parity on x86-64-kvm](benchmarks/runs/33959252438/x86-64-kvm-compute.svg)

| Workload | Helios | Linux + Wasmtime | Native Linux | vs Wasmtime | vs native |
| --- | ---: | ---: | ---: | ---: | ---: |
| `quickjs-loop` (headline) **parity bug** | 43.0 [41.0, 45.0] | 26.8 [26.6, 27.1] | 31.1 [31.0, 31.2] | 0.62x | 0.72x |
| `cpython-json` (headline) **parity bug** | 312 [306, 314] | 95.3 [93.7, 95.8] | 40.8 [40.6, 41.1] | 0.30x | 0.13x |
| `cpython-regex` **parity bug** | 456 [456, 462] | 214 [212, 216] | 117 [116, 118] | 0.47x | 0.26x |
| `aot-curl` **parity bug** | 370 [348, 371] | 111 [111, 114] | n/a | 0.30x | n/a |
| `wasm-simd-lanes` **parity bug** | 11.0 [8.50, 11.0] | 5.77 [5.49, 5.91] | 1.00 [0.97, 1.10] | 0.52x | 0.09x |
<!-- helios-bench:end -->
