# Virtio device evaluations

Three virtio devices have been proposed as additions to the Helios I/O
surface: virtio-mem and virtio-pmem (#17), virtio-fs with a DAX window
(#18), and virtio-rtc (#22). This document records what each device
offers, whether it is reachable from the host matrix Helios is actually
developed and tested on, what building it would cost in this codebase,
and the resulting decision.

Every availability claim below was probed on the development host
(macOS 25.6, Apple Silicon, Homebrew `qemu` 10.2.2) or read out of the
QEMU source at the tag named next to it. Every spec citation is against
`virtio-v1.4-cs01`, the revision the OASIS `virtio-spec` repository
currently carries.

## Probe results

`-device help` for all three system emulators, filtered to the devices
under evaluation:

| Device | `qemu-system-aarch64` | `qemu-system-riscv64` | `qemu-system-x86_64` |
| --- | --- | --- | --- |
| `virtio-mem-pci` / `virtio-mem-device` | absent | absent | absent |
| `virtio-pmem` / `virtio-pmem-pci` | absent | absent | **present** |
| `vhost-user-fs-pci` / `vhost-user-fs-device` | absent | absent | absent |
| `virtio-rtc` / `virtio-rtc-pci` | absent | absent | absent |
| `vmclock` (ACPI, not virtio) | absent | absent | present |

Accelerators available on this host, from `-accel help`:

| Emulator | Accelerators |
| --- | --- |
| `qemu-system-aarch64` | `hvf`, `tcg` |
| `qemu-system-riscv64` | `tcg` |
| `qemu-system-x86_64` | `tcg` |

There is no HVF for x86 guests on Apple Silicon, so anything that is
x86-only in QEMU is also TCG-only here. §3.5 names arm64+HVF as the
canonical performance surface precisely because it is the only
hardware-accelerated one.

The gating in the QEMU source (tag `v10.2.0`, `hw/virtio/Kconfig`)
explains the table:

```
config VIRTIO_MEM
    bool
    default y
    depends on VIRTIO
    depends on LINUX
    depends on VIRTIO_MD_SUPPORTED
    depends on VIRTIO_MEM_SUPPORTED
    select VIRTIO_MD

config VIRTIO_PMEM
    bool
    default y
    depends on VIRTIO
    depends on VIRTIO_MD_SUPPORTED
    depends on VIRTIO_PMEM_SUPPORTED
    select VIRTIO_MD

config VHOST_USER_FS
    bool
    default y
    depends on VIRTIO && VHOST_USER
```

`VIRTIO_MEM` carries `depends on LINUX`, which is why it is missing from
every emulator in a macOS build. `VIRTIO_PMEM` has no host-OS dependency
but is gated on `VIRTIO_PMEM_SUPPORTED`, which only `hw/i386/Kconfig`
selects; `VIRTIO_MEM_SUPPORTED` is selected by `hw/arm/Kconfig`
(`ARM_VIRT`), `hw/i386/Kconfig` and `hw/s390x/Kconfig`, and by nothing in
`hw/riscv/Kconfig`. `VHOST_USER_FS` needs `VHOST_USER`, which a macOS
build does not have.

Both remaining machine models already carry a real RTC without any
virtio involvement. From `-machine virt,dumpdtb=…`:

- `qemu-system-aarch64 -machine virt`: `pl031@9010000`, compatible
  `arm,pl031`.
- `qemu-system-riscv64 -machine virt`: `rtc@101000`, compatible
  `google,goldfish-rtc`.
- `qemu-system-x86_64 -machine pc`: `mc146818rtc` on the ISA bus.

## 1. virtio-mem hot-plug and virtio-pmem DAX (#17)

### Current state in Helios

Guest memory is fixed at boot. `inspector/src/vm.rs:37-39` defaults every
architecture profile to `2G` and `inspector/src/vm.rs:1447` passes it
straight to `-m`; there is no `maxmem`, no `slots`, and no memory-device
plumbing anywhere in the inspector.

The kernel splits each boot region once, in
`kernel/src/lib.rs:801-844`. `split_bootstrap_memory_region` gives the
tail `len / USER_HEAP_REGION_FRACTION` (that constant is `2`, so half the
region) to the user pool and the head to the kernel heap. The two halves
are published through `UserMemoryPool::add_region`
(`kernel/src/memory/user.rs:33`) and `KernelPhysFrameAllocator::add_region`
(`kernel/src/memory/pmm.rs:54`). Both are one-way: they call
`buddy_system_allocator::LockedHeap::add_to_heap` and nothing else. The
crate at the pinned version (`buddy_system_allocator = "0.12.0"`,
`kernel/Cargo.toml:26`) exposes `add_to_heap` and `init` and has no
counterpart that removes a range from the heap.

Neither `virtio-mem` nor `virtio-pmem` appears anywhere in the
repository. `virtio/src/transport.rs:41-61` enumerates exactly the device
ids Helios claims — Network (1), Block (2), Entropy (4), 9P (9) — and
`DeviceType::from_id` returns `None` for everything else, so a transport
that sees id 24 or 27 rejects the function rather than binding a driver.

### What the devices offer

**virtio-mem** is device ID 24 (virtio 1.4 §5.15, "Memory Device"). It
manages one guest-physical region partitioned into blocks of
`virtio_mem_config.block_size` that are individually plugged or unplugged.
The config layout carries `block_size`, `node_id`, `addr`, `region_size`,
`usable_region_size`, `plugged_size` and `requested_size`; the device
signals demand by changing `requested_size` and the driver satisfies it
with `VIRTIO_MEM_REQ_PLUG` / `VIRTIO_MEM_REQ_UNPLUG` /
`VIRTIO_MEM_REQ_UNPLUG_ALL` / `VIRTIO_MEM_REQ_STATE` requests answered
with `VIRTIO_MEM_RESP_ACK` / `NACK` / `BUSY` / `ERROR`. The spec is
explicit that the region "is not exposed as RAM via other firmware / hw
interfaces (e.g., e820 on x86)" and that "the driver is responsible for
deciding how plugged memory blocks will be used" — exactly the shape that
would suit a growable user pool.

**virtio-pmem** is device ID 27 (virtio 1.4 §5.19, "PMEM Device"). It is
a byte-addressable persistent region plus one request queue whose only
request type is `VIRTIO_PMEM_REQ_TYPE_FLUSH`. The region is located
either through `struct virtio_pmem_config { le64 start; le64 size; }` or,
when `VIRTIO_PMEM_F_SHMEM_REGION (0)` is negotiated, through virtio
shared memory region ID 0 — in which case the driver "MUST query shared
memory ID 0 for the physical address ranges, and MUST NOT use `start` or
`stop`".

### Host and QEMU availability

virtio-mem is **not reachable from this host at all** (`depends on LINUX`).
On a Linux host it would be available for aarch64 `virt` and x86 `pc` and
absent for riscv64 `virt`, so it could never cover the full target matrix
`just check-target` gates on.

virtio-pmem **is** reachable, but only on x86. It instantiates cleanly on
macOS once the machine has a memory-device slot — the first attempt
failed with

```
qemu-system-x86_64: -device virtio-pmem-pci,memdev=pmem0,id=nv0: the
configuration is not prepared for memory devices (e.g., for memory
hotplug), consider specifying the maxmem option
```

and with `-m 512M,maxmem=2G,slots=4` plus a
`memory-backend-file,share=on` the device realises and `query-memory-devices`
reports `{"type": "virtio-pmem", "data": {"memdev": "/objects/pmem0", …}}`.
Since QEMU has no HVF for x86 guests on Apple Silicon, that path is
TCG-only on this host.

### What would have to be built

For virtio-mem plug/unplug:

- `virtio/src/transport.rs`: a `DeviceType::Memory = 24` variant.
- `virtio/src/mem.rs`: the config-space reader plus a single-request
  driver on the existing `InFlight` slot machinery, like `rng.rs`.
- `hal/`: a capability describing that the platform can acquire
  additional physical memory at runtime, phrased as a hardware property
  (per §1, not "supports virtio-mem").
- `kernel/src/memory/`: a *removal* contract on the user pool. This is
  the real work. `UserMemoryPool` and `KernelPhysFrameAllocator` are
  built on a buddy heap with no region-removal API, so unplug —
  the half of the definition of done that makes hot-plug more than a
  larger `-m` — cannot be expressed without replacing the pool allocator
  or wrapping it in block-level bookkeeping that can prove a whole
  virtio-mem block is free before answering the device.
- `inspector/src/vm.rs`: `maxmem`/`slots` plumbing and a
  `virtio-mem-pci` profile.

For a virtio-pmem-backed cwasm cache, additionally:

- Shared-memory-region discovery, which does not exist:
  `virtio/src/pci.rs:43-53` parses only `VIRTIO_PCI_CAP_COMMON_CFG`,
  `_NOTIFY_CFG`, `_ISR_CFG` and `_DEVICE_CFG`, and there is no
  `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` handling; `virtio/src/mmio.rs`
  has no `SHMSel`/`SHMLen`/`SHMBase` registers.
- A `hal` device-memory mapping contract. `AddressSpace::commit`
  (`hal/src/vmm.rs:227`) materialises frames from the address space's own
  pool; there is no way to map a caller-supplied physical range, which is
  what #5 would add.

### Benefit

For virtio-mem, none that is currently measurable. The user pool is half
of a 2 GiB default allocation and no workload in the repository has been
shown to exhaust it; §3 is explicit that inflating memory to mask an
allocator or lifecycle bug is not allowed, so "we could grow the pool"
is not on its own a reason to build the device.

For virtio-pmem as a cwasm store, the theoretical win is skipping the
copy of a precompiled artifact into kernel memory before
`Engine::deserialize`. That win is bounded by what the kernel must do to
the bytes anyway: an artifact that is not bootfs-provisioned goes through
`verify_signed_artifact`
(`kernel/src/wasmtime_adapter/cwasm.rs:53-59`), which Ed25519-verifies
the whole payload and therefore reads every byte regardless of where the
bytes live. Mapping instead of copying saves one pass over the buffer,
not the pass that dominates.

### Risks

- Splitting the memory story across a device that exists on two of the
  three targets, and on neither of them under the accelerator the
  performance baseline uses, means the hot-plug path would be exercised
  only under TCG or not at all — a §3.4 SMP-correctness liability on a
  subsystem (the frame allocator) where every other processor's TLB is
  involved.
- Unplug is a use-after-free hazard by construction: the
  `PhysFrameAllocator::deallocate` contract already says a returned frame
  must have no live mapping and a completed shootdown. Answering
  `VIRTIO_MEM_REQ_UNPLUG` means proving that for a whole block.
- A persistent cwasm store on virtio-pmem adds a second trust boundary
  for signed artifacts on a memory region the host can mutate under the
  guest, with no additional isolation over the existing signed-artifact
  path.

### Decision

**virtio-mem: adopt later when** two conditions both hold — (a) the
user-memory pool grows a region-removal contract, so `UNPLUG` can be
answered rather than stubbed, and (b) a Linux QEMU host is part of the
check matrix, so the driver can be exercised at all. Neither holds today,
and a plug-only driver is the "degraded variant" §3.6 forbids.

**virtio-pmem DAX for the cwasm cache: reject because** the device is
x86-only in QEMU (`hw/i386/Kconfig` is the sole
`select VIRTIO_PMEM_SUPPORTED`), x86 has no accelerator on the
development host, and the cwasm path is signature-verified end to end, so
a zero-copy mapping removes one copy from a path whose cost is dominated
by a full-payload cryptographic read. It would also require shared-memory
region support in both transports and a device-memory mapping contract in
`hal/` that does not exist yet, for a benefit that cannot be measured on
the canonical arm64+HVF surface.

## 2. virtio-fs with a DAX window (#18)

### Current state in Helios

The host share is 9p. `inspector/src/vm.rs:1928-1941` builds
`-fsdev local,id=hostfs,path=…,security_model=none,multidevs=remap` and
attaches it to `virtio-9p-device` (aarch64, riscv64) or `virtio-9p-pci`
(x86). The guest side is the in-kernel 9p client at
`kernel/src/host_fs/client.rs`, speaking 9P2000.L behind the
`HostFileSystem` trait (`kernel/src/runtime/types.rs:899`).

Two properties of that client matter for this evaluation:

- The negotiated msize is already large. `P9_REQUESTED_MSIZE` is
  `(1024 * 1024) + 24` (`kernel/src/host_fs/client.rs:54`), so the
  payload chunk is 1 MiB minus the fixed header — eight times the Linux
  9p default.
- There is no cache. `read_file_impl`
  (`kernel/src/host_fs/client.rs`) does `walk` → `get_attr` →
  `read_file_all` (which opens, then loops `Tread` at
  `session.payload_chunk()` granularity) → `clunk`, every time, and the
  string `cache` does not appear in the file.

`HostFileSystem::read_file` returns `Vec<u8>` by contract. Every read
through the host share is an owned, copied buffer by definition of the
trait.

### What the device offers

virtio-fs is device ID 26 (virtio 1.4 §5.11, "File System Device"). It
transports FUSE requests over virtqueues — a hiprio queue, an optional
notification queue behind `VIRTIO_FS_F_NOTIFICATION (0)`, and
`num_request_queues` request queues — with the device acting as the FUSE
daemon and the driver as the FUSE client.

The DAX window is §5.11's "Device Operation: DAX Window". Shared memory
region ID 0 is the window; the driver maps a file range into it with
`FUSE_SETUPMAPPING` and removes it with `FUSE_REMOVEMAPPING`. The spec is
explicit that "Providing the DAX Window is optional for devices" and that
"the driver SHOULD be prepared to find shared memory region ID 0 absent
and fall back to FUSE_READ and FUSE_WRITE requests". §5.11 also notes the
window's own security implication: it "provides direct" access to host
page cache state, which is a side channel the 9p path does not have.

### Host and QEMU availability

QEMU implements virtio-fs only as `vhost-user-fs-pci` /
`vhost-user-fs-device`, i.e. a vhost-user transport to an external
daemon. Neither device exists in any of the three emulators on this host,
because `VHOST_USER_FS depends on VIRTIO && VHOST_USER` and vhost-user is
not built on macOS. There is no `virtiofsd` in Homebrew and none on this
machine (`which virtiofsd` → not found); virtiofsd is a Linux daemon.

The DAX window itself was never merged into upstream QEMU. It has lived
in the `virtio-fs/qemu` fork since the 5.0 era, and the upstream
vhost-user implementation of the window is still incomplete.

So on the supported host matrix: unavailable on macOS for all three
targets, available on Linux only as vhost-user + virtiofsd and only
without DAX unless a forked QEMU is used.

### Testing the premise

The issue's premise is that virtio-fs with DAX would avoid "`msize`-bounded
9p round trips" for the CPython stdlib, cwasm caches and bootfs programs.
Measured against the staged tree (`artifacts/python3-root`, CPython
3.14.4 WASI as `tools/wasi-apps/build.sh` produces it):

| Metric | Value |
| --- | --- |
| Files | 559 |
| Total bytes | 18,539,768 |
| Median file size | 10,195 B |
| p90 file size | 46,807 B |
| Largest file | 7,890,383 B (`python3.wasm`) |
| Files larger than the 1 MiB payload chunk | 1 |

Every file except `python3.wasm` fits in a single `Tread`. A stdlib
import therefore costs `Twalk` + `Tgetattr` + `Tlopen` + one `Tread` +
`Tclunk` — five round trips of which exactly one is bounded by msize, and
that one is not saturated. `python3.wasm` is the only artifact where
msize matters, and it costs eight `Tread`s.

The premise does not hold. The per-file cost on this workload is fixed
protocol round trips and the absence of any kernel-side cache, not the
transfer size. A DAX window removes the copy, not the four metadata
round trips.

### What would have to be built

- A FUSE client in `kernel/`, implementing `HostFileSystem` — the whole
  of `FUSE_INIT`/`LOOKUP`/`OPEN`/`READ`/`WRITE`/`GETATTR`/`SETATTR`/
  `READDIRPLUS`/`FORGET` and the hiprio queue's
  `FUSE_INTERRUPT`/`FORGET`/`BATCH_FORGET`, plus `FUSE_SETUPMAPPING` and
  `FUSE_REMOVEMAPPING` for DAX. That is a larger protocol surface than
  the 1,903-line 9p client it would sit beside.
- `DeviceType::FileSystem = 26` in `virtio/src/transport.rs`, and a
  multi-queue driver (§5.11: one hiprio, one optional notification,
  `num_request_queues` request queues).
- Shared-memory-region discovery in both transports, which as noted in §1
  does not exist in `virtio/src/pci.rs` or `virtio/src/mmio.rs`.
- A `hal/` device-memory contract plus the mapping path from #5, and a
  new borrowed-slice method on `HostFileSystem` — the existing
  `read_file -> Vec<u8>` shape cannot express a mapping, so surfacing
  DAX means changing the trait every backend implements.
- `inspector/src/vm.rs`: a `vhost-user-fs` host-share profile and daemon
  lifecycle management, which only works on a Linux host.

Per §3 the 9p path may not be replaced without explicit approval, so this
would be additive and selectable per VM, doubling the host-share surface
that `just check-host` and the embedded-debugger test have to cover.

### Benefit

Not measurable from here, and the mechanism the issue credits for the
benefit is not the one the workload is bound by. The realistic upside of
virtio-fs on the CPython import workload comes from FUSE's cacheable
lookup/attribute model and virtiofsd's own caching, neither of which
requires DAX — and both of which are equally available to the existing
9p client, which today re-walks and re-stats every path on every call.

### Risks

- A second host-share transport that cannot be exercised on the primary
  development host is a path that rots. The kernel-debug and
  boot-evidence workflows in §5 all run under the local QEMU.
- The DAX window is an optional device feature not present upstream, so
  the code path that motivates the whole change would be dead on any
  stock QEMU.
- §5.11's own security note: the DAX window exposes host page-cache
  residency to the guest. The 9p path does not.

### Decision

**Reject because** the stated premise is false on the measured workload —
the 9p client already negotiates a 1 MiB msize and 558 of the 559
CPython-root files fit in one chunk, so the cost is fixed protocol round
trips, not msize-bounded transfers — and because neither the device nor
its DAX window is reachable from the supported host matrix (no
`vhost-user-fs` on macOS, no `virtiofsd` on macOS, no DAX window
upstream). The work that would actually move the CPython import workload
is a metadata and content cache in `kernel/src/host_fs/client.rs`, which
needs no new device, no new transport feature, no `hal/` mapping
contract, and no change to `HostFileSystem`.

## 3. virtio-rtc as the system-clock source (#22)

### Current state in Helios

There is no RTC driver of any kind. Grepping the backends for `rtc`,
`pl031`, `goldfish` or `cmos` returns nothing outside unrelated
identifiers, and `virtio/src/transport.rs:41-61` does not list device
ID 17.

`KernelClock` (`kernel/src/exec/time.rs`) is monotonic-only plus a
software offset. `KernelClock::new` sets `wall_clock_offset_nanos: 0`
(`kernel/src/exec/time.rs:26`), `monotonic_nanos` reads
`ComponentRuntimeState::uptime_nanos(cpu.now().ticks())`, and
`system_time_nanos` is monotonic + offset. The only way that offset ever
becomes non-zero is a guest calling `set_system_time_nanos` while holding
a `SetWallClockCap` — WASIX `clock_time_set`
(`kernel/src/wasmtime_adapter/component_host/service/wasix_proc.rs:1047`)
or the preview1 shim
(`kernel/src/wasmtime_adapter/component_host/service/preview1.rs:141-145`).

So `wasi:clocks/wall-clock` (`kernel/src/wasmtime_adapter/wasi/preview2.rs:1143-1155`)
reports uptime-since-boot as if it were the Unix epoch until something
inside the guest corrects it. Nothing in the boot path seeds it: the
inspector passes no epoch to the guest, and none of the three machine
models' RTCs is read.

The netstack does not use wall-clock time at all. TCP timestamps in
`netstack/src/packet.rs` are RFC 7323 option values and the RTO
estimator in `netstack/src/tcp.rs:523-569` works entirely in
monotonic nanoseconds.

### What the device offers

virtio-rtc is device ID 17 (virtio 1.4 §5.23, "RTC Device"). Two
virtqueues: requestq (0), and alarmq (1) which "exists only if
VIRTIO_RTC_F_ALARM has been negotiated". One feature bit,
`VIRTIO_RTC_F_ALARM (0)`.

Clock types are `VIRTIO_RTC_CLOCK_UTC (0)`, `VIRTIO_RTC_CLOCK_TAI (1)`,
`VIRTIO_RTC_CLOCK_MONOTONIC (2)`, `VIRTIO_RTC_CLOCK_UTC_SMEARED (3)` and
`VIRTIO_RTC_CLOCK_UTC_MAYBE_SMEARED (4)`, with leap-second smearing
described by `VIRTIO_RTC_SMEAR_UNSPECIFIED (0)`,
`VIRTIO_RTC_SMEAR_NOON_LINEAR (1)` and `VIRTIO_RTC_SMEAR_UTC_SLS (2)`.

Control requests are `VIRTIO_RTC_REQ_CFG (0x1000)`,
`VIRTIO_RTC_REQ_CLOCK_CAP (0x1001)` and
`VIRTIO_RTC_REQ_CROSS_CAP (0x1002)`; read requests are
`VIRTIO_RTC_REQ_READ (0x0001)` and `VIRTIO_RTC_REQ_READ_CROSS (0x0002)`;
alarms are `VIRTIO_RTC_REQ_READ_ALARM (0x1003)`,
`VIRTIO_RTC_REQ_SET_ALARM (0x1004)` and
`VIRTIO_RTC_REQ_SET_ALARM_ENABLED (0x1005)`.

`VIRTIO_RTC_REQ_READ_CROSS` is the interesting one: it "returns a
cross-timestamp for the clock identified by the `clock_id` field",
pairing a device clock reading with a guest-readable hardware counter —
`VIRTIO_RTC_COUNTER_ARM_VCT (0)` for `CNTVCT_EL0`,
`VIRTIO_RTC_COUNTER_X86_TSC (1)` for the TSC,
`VIRTIO_RTC_COUNTER_INVALID (0xFF)`. The spec footnotes it as "similar to
the ptp_kvm mechanism in the Linux kernel". That correlation is exactly
what a monotonic↔wall cross-timestamp needs, and it is the one thing no
legacy RTC can provide. There is no RISC-V counter identifier in the
list.

### Host and QEMU availability

**Not available on this host.** QEMU 10.2.2 has no `virtio-rtc` device on
any of the three targets, and `hw/virtio/virtio-rtc.c` does not exist at
tag `v10.2.0`.

It was added upstream on 2026-02-28 ("virtio-rtc: Add basic virtio-rtc
support") and first appears in a release at `v11.1.0`. Its Kconfig entry
is unusually permissive:

```
config VIRTIO_RTC
    bool
    default y
    depends on VIRTIO
```

No host-OS dependency and no per-machine `_SUPPORTED` gate, so on QEMU
≥ 11.1.0 the device would be present for aarch64, riscv64 and x86 alike,
on macOS as well as Linux — the only one of the three devices in this
document that could cover the whole target matrix.

The implementation, however, is minimal. Reading
`hw/virtio/virtio-rtc.c` at `v11.1.0`:

- `virtio_rtc_device_realize` calls `virtio_init(vdev, VIRTIO_ID_CLOCK, 0)`
  and adds exactly **one** queue of size 64. There is no alarmq.
- `virtio_rtc_get_features` returns the input mask unchanged, so
  `VIRTIO_RTC_F_ALARM` is never offered.
- `VIRTIO_RTC_REQ_CFG` reports `num_clocks = 1`;
  `VIRTIO_RTC_REQ_CLOCK_CAP` reports that single clock as
  `VIRTIO_RTC_CLOCK_UTC`.
- `VIRTIO_RTC_REQ_CROSS_CAP` answers `VIRTIO_RTC_S_OK` with a
  zero-initialised response body, i.e. no supported hardware counter.
- `VIRTIO_RTC_REQ_READ` returns `qemu_clock_get_ns(QEMU_CLOCK_HOST)`.
- `VIRTIO_RTC_REQ_READ_CROSS` and every alarm request fall through to
  the `default` arm and are answered `VIRTIO_RTC_S_EOPNOTSUPP`.

So against upstream QEMU today, virtio-rtc delivers a single UTC
nanosecond read over a virtqueue round trip, and neither of the two
capabilities the issue's definition of done names — cross-timestamps for
the netstack, and alarms — is implemented.

### What would have to be built

- `virtio/src/transport.rs`: `DeviceType::Rtc = 17`.
- `virtio/src/rtc.rs`: a single-request driver on the existing `InFlight`
  slot pattern (`rng.rs` is the template), doing `REQ_CFG` → `CLOCK_CAP`
  → optional `CROSS_CAP` at bring-up and `REQ_READ` / `REQ_READ_CROSS`
  afterwards.
- `hal/`: a wall-clock source contract, phrased as the hardware property
  (a device that reports civil time, optionally correlated with the
  platform's cycle counter) rather than naming virtio — §1. The
  cross-timestamp counter ids map onto `CNTVCT_EL0` and the TSC, which
  are already what `Cpu::now()` reads on aarch64 and x86; riscv64 has no
  counter id in the spec, so the correlated path would be arm64/x86 only
  even in a complete implementation.
- `kernel/src/exec/time.rs`: seed `wall_clock_offset_nanos` at boot from
  that source instead of leaving it `0`, and keep
  `SetWallClockCap` as the only way to move it afterwards.
- `inspector/src/vm.rs`: a `virtio-rtc` device profile per architecture.

### Benefit

Two separable things:

1. **A correct wall clock at boot.** Real and currently missing —
   `wasi:clocks/wall-clock` reports uptime as epoch time until a guest
   corrects it. But virtio-rtc is not needed for this. Every machine
   model Helios boots already has an RTC (PL031 on aarch64 `virt`,
   `google,goldfish-rtc` on riscv64 `virt`, `mc146818rtc` on x86 `pc`),
   all three are discoverable from the device tree or ACPI the backends
   already parse, and each is a single MMIO/port read with no virtqueue,
   no feature negotiation and no driver in the I/O path. Against those,
   QEMU 11.1's virtio-rtc is strictly more machinery for the same single
   UTC read.

2. **A monotonic↔wall cross-timestamp.** This is what only virtio-rtc
   can give, via `VIRTIO_RTC_REQ_READ_CROSS`, and it is the part QEMU
   does not implement. The netstack does not need it — its RTO estimator
   and TCP timestamps are monotonic-only — so the beneficiary would be
   future PTP-grade timestamping, not current code.

### Risks

- Building against a device whose only shipping implementation answers
  `S_EOPNOTSUPP` to the two requests that justify it means writing code
  paths with no way to test them.
- The host QEMU is 10.2.2; adopting virtio-rtc raises the minimum QEMU
  for the inspector's default profiles across every developer machine and
  CI runner.
- A virtqueue in the clock path is a latency and failure surface a
  register read does not have, and the clock is consulted on the WASI
  timer path.

### Decision

**Adopt later when the QEMU on the supported host matrix is ≥ 11.1.0
*and* its virtio-rtc implements `VIRTIO_RTC_REQ_READ_CROSS` with a real
hardware-counter id.** Until the cross-timestamp exists, virtio-rtc is a
strictly more expensive way to read the same UTC value that PL031,
goldfish-rtc and mc146818 already expose on the exact machines Helios
boots, and `VIRTIO_RTC_F_ALARM` — the other half of the definition of
done — is not offered by any shipping device.

The gap the issue correctly identifies is real and independent of the
device: `wall_clock_offset_nanos` starts at `0` and nothing seeds it, so
`wasi:clocks/wall-clock` is wrong on every boot. Closing that belongs to
a `hal/` wall-clock source contract backed by the RTC each backend's
machine already provides — the same contract a virtio-rtc driver would
later implement as one more source.

## Summary

| Issue | Device | Decision |
| --- | --- | --- |
| #17 | virtio-mem | adopt later when the user-memory pool supports region removal and a Linux QEMU host is in the check matrix |
| #17 | virtio-pmem (DAX cwasm cache) | reject — x86-only in QEMU, no accelerator on the dev host, and the cwasm path is signature-verified end to end |
| #18 | virtio-fs with DAX | reject — premise falsified by the measured workload; device and DAX window both unreachable from the supported host matrix |
| #22 | virtio-rtc | adopt later when QEMU ≥ 11.1.0 is the floor and `REQ_READ_CROSS` is implemented with a real counter id |

## References

- Virtual I/O Device (VIRTIO) Version 1.4, Committee Specification 01 —
  §5.11 File System Device, §5.15 Memory Device, §5.19 PMEM Device,
  §5.23 RTC Device. Device-type ids from the table in §5.1.
  <https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.html>
- QEMU `hw/virtio/Kconfig`, `hw/arm/Kconfig`, `hw/i386/Kconfig`,
  `hw/riscv/Kconfig` at tag `v10.2.0`.
- QEMU `hw/virtio/virtio-rtc.c` at tag `v11.1.0`.
- QEMU 10.2.2 (Homebrew, macOS/arm64) `-device help`, `-accel help`,
  `-object help`, `-machine virt,dumpdtb=…`, and a live
  `virtio-pmem-pci` realisation checked through QMP `query-memory-devices`.
