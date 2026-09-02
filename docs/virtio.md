# Virtio in helios

Helios drives virtio devices over two transports — modern virtio-mmio
(version 2) and modern virtio-pci — and over both virtqueue layouts. The
same drivers run on either layout; nothing in `kernel/`, `riscv/`,
`x86/`, `aarch64/` or `hosted/` is instantiated twice.

## Feature negotiation

`virtio/src/features.rs` owns the status and feature handshake. Every
driver calls `negotiate(&transport, RING_FEATURES | <device bits>)`,
stores the resulting `NegotiatedFeatures`, and passes it to
`VirtQueue::new`. The ring features helios always asks for are:

| Feature | Bit | Effect when the device offers it |
| --- | --- | --- |
| `VIRTIO_F_VERSION_1` | 32 | Mandatory. Negotiation fails without it. |
| `VIRTIO_F_INDIRECT_DESC` | 28 | Chains of two or more buffers move into a pre-allocated per-slot indirect table and cost one ring descriptor. |
| `VIRTIO_F_RING_EVENT_IDX` | 29 | Both sides publish the index they want their next notification at, suppressing kicks and interrupts. |
| `VIRTIO_F_RING_PACKED` | 34 | The queue uses the packed ring layout of virtio 1.1. |
| `VIRTIO_F_IN_ORDER` | 35 | The device may report a batch of completions with a single used entry. |
| `VIRTIO_F_NOTIFICATION_DATA` | 38 | Queue kicks carry the ring position the driver has published up to. |
| `VIRTIO_F_RING_RESET` | 40 | A single queue can be reset and re-programmed without resetting the device. |

The accepted set is logged at `info` once per device, so a boot log shows
exactly what a given QEMU version offered:

```
virtio features negotiated device=Network ring=split indirect=true event_idx=true
  in_order=false notification_data=false ring_reset=true offered=… accepted=…
```

`VIRTIO_F_RING_RESET` is only reachable through a transport register.
The virtio-mmio register layout defines none, so `negotiate` masks the
bit out for MMIO devices rather than claiming a feature the driver could
not honour; virtio-pci uses `queue_reset` in the extended common
configuration structure.

## Ring layouts

`VirtQueue` wraps a private enum with one variant per layout. This is the
one place an enum is the right tool: the layout is a device capability
discovered at runtime, and making the drivers generic over it would force
each backend to instantiate every driver twice.

- **Split** (`virtio/src/queue/split.rs`) — descriptor table, driver-owned
  available ring, device-owned used ring. Descriptor identifiers are
  table indices.
- **Packed** (`virtio/src/queue/packed.rs`) — one descriptor ring plus two
  event-suppression structures. Chains carry a driver-chosen buffer id and
  become available when the head descriptor's AVAIL/USED flag pair is
  written to the driver's wrap counter.

Identifiers come from a first-in first-out pool in both layouts. That is
what makes `VIRTIO_F_IN_ORDER` expressible: the feature requires the
driver to consume the descriptor table in ring order, and a queue whose
completions arrive in submission order returns identifiers to the tail in
the order they left the head.

Completions are always routed by identifier. No driver assumes the
completion it observes belongs to the request it submitted; the single
request drivers register a slot in `InFlight` and any woken task drains
the queue on everyone's behalf.

## Exercising the layouts under QEMU

QEMU creates virtio devices with `packed=off` and `in_order=off`, so the
guest cannot reach either path on its own — the VM has to be built for
it. The inspector exposes both as flags that apply to every virtio
device it creates, and they compose:

```bash
# Packed ring.
cargo run -p helios-inspector -- vm --arch aarch64 --virtio-packed \
    --boot-program dash --boot-program debugger --no-compiler-plugin \
    shell -c 'echo ok'

# Split ring with batched in-order completions.
cargo run -p helios-inspector -- vm --arch aarch64 --virtio-in-order \
    --boot-program dash --boot-program debugger --no-compiler-plugin \
    shell -c 'echo ok'

# Both at once.
cargo run -p helios-inspector -- vm --arch aarch64 \
    --virtio-packed --virtio-in-order \
    --boot-program dash --boot-program debugger --no-compiler-plugin \
    shell -c 'echo ok'
```

The same switches are available in a VM config file as `virtio_packed`
and `virtio_in_order`. `indirect_desc`, `event_idx` and `queue_reset` are
QEMU defaults and need no flag. `notification_data` has no QEMU property
at all — QEMU does not implement VIRTIO_F_NOTIFICATION_DATA — so that
path is covered by the unit tests in `virtio/src/queue/tests.rs` rather
than under the VM.

## Devices

`DeviceType` lists exactly the virtio device kinds a Helios driver
claims: network (1), block (2), entropy (4) and 9P (9). A transport that
reads any other device id rejects the function rather than mapping it to
a placeholder driver.

Four further device kinds have been evaluated and deliberately not
claimed — RTC (17), memory (24), file system (26) and PMEM (27).
`docs/virtio-evaluations.md` records, for each one, what the device
offers with virtio 1.4 spec citations, what QEMU actually exposes on the
supported host matrix as probed, what building the driver would cost in
this repository, and the decision.

virtio-console (3) is deliberately absent. Every backend's console is the
platform UART, which has to work before the allocator exists and on the
panic path, so a virtio console could only ever be a second port on an
already-working terminal; structured host↔guest transport belongs to
vsock instead. The driver that used to sit in `virtio/src/console.rs` had
no callers and was removed rather than kept as dead weight.

virtio-net is the one device whose capabilities are decided outside the
guest: multiqueue, segmentation offload and checksum offload are all
properties of the host packet path QEMU is given, so what the driver can
negotiate depends on `helios-inspector vm --net-backend`. The driver logs
its device-class result as `virtio-net online` next to the generic
feature line above. See `docs/networking.md`.

virtio-entropy is the kernel's continuous entropy source. The driver in
`virtio/src/rng.rs` is interrupt-driven like every other single-request
driver: `fill` submits a writable buffer, registers an `InFlight` slot
and parks on the device notification, and a zero-length completion is a
device fault. The kernel mixes what it reads into its root DRBG; see
`kernel/src/memory/entropy.rs`.

virtio-blk is the kernel's disk. The driver in `virtio/src/block.rs`
negotiates the whole feature set QEMU offers and turns it into a
`hal::fs::BlockGeometry` plus a capability set, so callers address the
device in its own logical blocks rather than in 512-byte sectors it may
not use natively:

| Feature | Bit | Effect when the device offers it |
| --- | --- | --- |
| `VIRTIO_BLK_F_SIZE_MAX` | 1 | Bounds the bytes one buffer of a request may carry. |
| `VIRTIO_BLK_F_SEG_MAX` | 2 | Bounds the buffers one request is scattered across. |
| `VIRTIO_BLK_F_RO` | 5 | Writes, discards and write-zeroes are refused before they reach the device. |
| `VIRTIO_BLK_F_BLK_SIZE` | 6 | `block_size()` reports the device's logical block; addresses are converted to sectors on the wire. |
| `VIRTIO_BLK_F_FLUSH` | 9 | `flush()` commits the volatile write cache. Without the bit there is no such cache, and `flush()` resolves without reaching the device. |
| `VIRTIO_BLK_F_TOPOLOGY` | 10 | Physical block size, minimum and optimal I/O reach the geometry. |
| `VIRTIO_BLK_F_CONFIG_WCE` | 11 | The current write-cache mode is read at bring-up and logged. |
| `VIRTIO_BLK_F_MQ` | 12 | One queue per processor, up to what the device exposes; a request is bound to the queue it was written into. |
| `VIRTIO_BLK_F_DISCARD` | 13 | `discard(range)` tells the device the blocks are free. |
| `VIRTIO_BLK_F_WRITE_ZEROES` | 14 | `write_zeroes(range)` zeroes a run without carrying the zeroes. |

Requests are pipelined: each queue keeps up to 128 chains in flight, each
with its own `InFlight` slot, and a submitter that finds the ring full
drains what the device published and parks on the device notification
rather than failing. Transfers longer than `SEG_MAX × SIZE_MAX` are split
into whole-block requests, and every read checks the used length against
what it asked for.

`VIRTIO_BLK_T_GET_ID` is what tells two disks apart. A VM hands the guest
both the image its firmware booted from and the scratch disk the kernel
owns, on the same bus and in an order nothing guarantees, so the kernel
identifies its disk by the serial the inspector gives it —
`helios-data` — and leaves every other disk untouched. The chosen disk is
then proved before anything depends on it: a random 4 KiB pattern goes to
its last blocks, is committed with a flush, read back and compared, and
released with write-zeroes. A mismatch is a fatal boot error, and the
result is visible in the boot log:

```
virtio-blk configured capacity_blocks=524288 block_bytes=512 queues=1 queue_depth=128
  segments=14 flush=true discard=true write_zeroes=true writeback=true
block device identified as the kernel scratch disk serial="helios-data"
block device online, self check passed capacity_bytes=268435456 …
```

Every inspector profile attaches that disk: `virtio-blk-device` on the
MMIO platforms (aarch64, riscv64) and `virtio-blk-pci` on x86, always
with `serial=helios-data`, backed by a `data.img` in the VM's runtime
directory whose size `--data-disk-size` controls. `helios-inspector
stats` shows the device the guest kernel ended up with, its geometry, its
queues and the requests the kernel has issued.

## Confined DMA: virtio-iommu and `VIRTIO_F_ACCESS_PLATFORM`

Without a translation unit a virtio device reads and writes physical
memory directly: the descriptor rings carry physical addresses, and
every device on the bus can reach every byte of the machine. The x86
platform can put its devices behind a virtio-iommu instead, and then a
device reaches only the ranges the kernel mapped into its own domain.

The layering follows the usual one:

- `hal/src/iommu.rs` holds the contract only — endpoint and domain
  identities, access rights, unit geometry, the `Iommu` trait, and
  `DmaTranslation`, the value that turns a physical address into the
  address a device has to issue for it. `DmaModel::Iommu` is the platform
  fact that says a machine has one.
- `virtio/src/iommu.rs` drives the device (id 23): the request queue
  carries `ATTACH`/`DETACH`/`MAP`/`UNMAP`/`PROBE`, the event queue
  carries translation faults. Requests are issued from bring-up and
  teardown, never from a data path.
- `kernel/src/io/iommu.rs` decides the policy: one domain per device,
  one slot of the I/O virtual address space per domain, placed above
  every interrupt doorbell each domain identity-maps. A firmware memory
  map with more runs than a domain has windows is folded down by merging
  across the smallest gaps first.
- `x86/src/iommu.rs` finds the unit. q35 publishes the topology in the
  ACPI VIOT table: one node names the PCI function the virtio-iommu sits
  on, and PCI range nodes map bus/device/function numbers onto endpoint
  identities. The vendored `acpi` crate has no VIOT support and cannot
  be given one — its `Signature` type has no public constructor — so the
  table is located by walking the SDT headers and parsed here.

Every driver publishes its addresses through a `PlatformDmaPool`, which
wraps the backend's ordinary pool with the domain's `DmaTranslation`. A
driver never knows the difference; a buffer whose physical address the
domain does not map is refused by name at submission instead of faulting
inside the device. Because the addresses on the wire are no longer
physical, the device has to be told: `negotiate` adds
`VIRTIO_F_ACCESS_PLATFORM` for any device whose pool hands out
translated addresses, refuses a device that cannot support the feature,
and refuses the mirror case — a device that demands the feature on a
machine where the kernel built it no domain.

Each domain's slot starts at a nonzero I/O virtual address, so this is
not identity mapping under another name: if the kernel published a
physical address anywhere the device would fault on its first fetch.

Only PCI endpoints can be confined. virtio-iommu translates the DMA of
functions on a PCI bus; a memory-mapped virtio device is not an endpoint
of anything, so the aarch64 and riscv64 `virt` profiles — whose virtio
devices are on the MMIO transport — have no translation unit and their
devices keep reaching all of memory. `helios-inspector vm --iommu`
refuses those architectures by name rather than accepting the flag and
doing nothing. This is a property of the transport, not a gap in the
driver: putting those platforms behind a unit means moving their devices
onto PCI first.

```bash
cargo run -p helios-inspector -- vm --arch x86-64 --iommu \
    --boot-program dash --boot-program debugger --no-compiler-plugin \
    shell -c 'echo ok'
```

The flag attaches a `virtio-iommu-pci` unit — realised before the
functions it protects, because QEMU binds a function to its address
space when the function is created — and creates every other virtio-PCI
function with `iommu_platform=on` and `disable-legacy=on`; only a
non-transitional function offers `VIRTIO_F_ACCESS_PLATFORM` at all. The
unit itself is never one of its own endpoints: it publishes its request
and event rings at physical addresses.

The boot log shows one line per confined device, and the feature line of
each device then reports `access_platform=true`:

```
virtio-iommu online function=00:01.0 msix_vector=55 global_bypass=true
virtio device confined to its own IOMMU domain endpoint=0x10 domain=0
  iova_base=… mapped_bytes=… granule=…
virtio features negotiated device=_9P … access_platform=true
```

`helios-inspector stats` carries the same facts in its IOMMU panel: the
unit's granule, whether endpoints outside every domain still reach
memory, the running fault count, and the domain and mapped bytes of each
confined device. Faults arrive on the unit's own MSI-X vector, so a
device that issues an address its domain does not map is reported with
the endpoint and the address that caused it rather than failing
silently.

The global bypass state is about the *rest* of the machine: an endpoint
attached to a domain is always translated, and bypass only decides
whether a device the kernel never claimed can still reach memory. QEMU
leaves it on at reset, which is what keeps the firmware's own use of the
boot disk working before the kernel takes over.
