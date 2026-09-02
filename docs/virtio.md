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
