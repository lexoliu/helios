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

Every side gets the same QEMU release, accelerator (KVM on Linux hosts,
HVF on Apple Silicon), vCPU count, memory, virtio block/net/rng devices,
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
`null` counterpart is reported as uncovered, never approximated.

| Class | Workload | Helios | Linux + Wasmtime | Native Linux |
| --- | --- | --- | --- | --- |
| startup | `instance-startup-{1,100,500}` | `procbench startup N hello hold` through `helios:system/programs`; time to first stdout byte per instance, memory per instance from `helios:system/stats` and `instances` while all are alive | `procbench` spawning `wasmtime run --allow-precompiled hello.cwasm hold` | `procbench` spawning the C `hello`; RSS from `/proc` |
| startup | `spawn-wait` | 200 sequential spawn+wait | same, Wasmtime child | same, native child |
| startup | `process-startup` | 20 × `dash -c true` | — | same |
| hostcall | `hostcall-loop` | 2 000 000 × `wasi:clocks/monotonic-clock.now` | same wasm | 2 000 000 × `clock_gettime(CLOCK_MONOTONIC)` |
| ipc | `pipe-pingpong` | 20 000 × 64-byte round trip through a child's stdin/stdout | Wasmtime `pipe-echo` child | C `pipe-echo` child |
| ipc | `pipe-stream` | 64 MiB through the child | same | same |
| ipc | `stdio-pipe` | coreutils pipeline | — | same |
| sched | `sched-tasks` | 64 cooperative tasks × 2000 `yield_now` (one host call each) | — (Wasmtime has no cooperative scheduler for a CLI program) | 64 threads × 2000 `sched_yield` |
| net | `tcp-throughput`, `tcp-upload`, `wasi-tcp-throughput`, `wasix-tcp-throughput`, `curl-*` | 64 MiB streams through the in-kernel stack | `wasi-tcp-throughput` only | Python client / curl |
| net | `tcp-latency` | 5000 × 16-byte round trip to a host echo server | same wasm | C client with `TCP_NODELAY` |
| fs | `fs-smallfiles`, `fs-readstream` | coreutils on the embedded filesystem root | — | ext4 in the guest |
| compute | `quickjs-loop`, `cpython-json`, `cpython-regex`, `wasm-simd-lanes` | interpreter or SIMD loops | same wasm | native QuickJS/CPython/NEON-or-SSE probe |
| compute | `aot-curl` | compiler plugin AOT of `curl.wasm` | `wasmtime compile` of the same input | — |

`headline: true` marks the rows the README table and the regression gate
carry. Compute is a parity check, not a claim: Helios running the same
wasm on the same Cranelift must be within noise of Linux + Wasmtime, and
a significant loss there is flagged `parity_bug` in the report and filed
as a bug rather than reported as a number.

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
- The aarch64 shared runner is a slirp lane (single queue, no offload);
  its network numbers are comparable neither with the tap lane nor with
  the dedicated machine's vmnet-bridged backend.

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
- Machine noise is measured, not assumed: the `control_workload`
  (`quickjs-loop`) runs before and after the suite on every side, and the
  **noise floor** is the larger of the control's median drift and its CV.
- A comparison between Helios and a Linux side is **significant** when
  the two warm bootstrap intervals do not overlap and the ratio of medians
  moves by more than the noise floor; otherwise it prints "within noise".
- The regression gate compares a candidate report against the newest
  `dev` report of the same lane and calls a headline workload regressed
  when its Helios warm intervals are disjoint and the median moved by more
  than the larger of the two runs' noise floors. It blocks only when both
  reports are publishable, i.e. from dedicated runners; on advisory reports
  it comments the table on the pull request.

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

## Dedicated runners

Publishable numbers come from two self-hosted machines, one per lane,
registered with these labels:

| Lane | Label | Host | Accelerator |
| --- | --- | --- | --- |
| `x86-64-kvm` | `helios-bench-x86-kvm` | x86-64 Linux, `/dev/kvm` | KVM |
| `aarch64-hvf` | `helios-bench-arm-hvf` | Apple Silicon macOS | HVF |

Requirements the manifest's host check cannot verify and the machine's
owner must guarantee: the CPU frequency governor is fixed (`performance`
on Linux; on macOS the machine is on mains power and not in low-power
mode), turbo behaviour is the same for every run, nothing else is
scheduled on the machine while the workflow runs, the same QEMU release
is installed as the lane pins, and the network backend is the lane's
(`tap` with vhost-net on Linux, vmnet-bridged on macOS).

Until these machines exist, the same jobs run on GitHub's shared runners
in **advisory** mode: the report carries `publishable: false`, the README
says so, and nothing blocks. The repository variable
`HELIOS_BENCH_DEDICATED=true` turns on the dedicated-runner runs for
pushes to `dev`; tags always use the dedicated labels.

## Reproducing a published number

```bash
git checkout <tag>
cd tools/bench && uv sync
uv run helios-bench host-check --lane aarch64-hvf     # must print no deviation
uv run helios-bench run --lane aarch64-hvf --out-dir ../../target/bench/aarch64-hvf
uv run helios-bench render tables --report ../../target/bench/aarch64-hvf/report.json
```

`run --dry-run` prints the exact harness commands and the host
deviations without running anything. `--sides helios` or
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

<!-- helios-bench:begin run=pending -->
No advisory run has been rendered into this page yet.
<!-- helios-bench:end -->
