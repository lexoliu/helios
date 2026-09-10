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
claims: network (1), block (2), entropy (4), memory balloon (5), 9P (9),
GPU (16), input (18), vsock (19), IOMMU (23) and sound (25). A transport
that reads any other device id rejects the function rather than mapping
it to a placeholder driver.

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

virtio-vsock is the machine's link to whatever hosts it. The driver in
`virtio/src/vsock.rs` drives the three queues the device defines —
receive, transmit, and an event queue carrying
`VIRTIO_VSOCK_EVENT_TRANSPORT_RESET` — and reads the guest's context id
out of the configuration space. The receive ring is also its whole
receive buffer pool: one page per descriptor, allocated at bring-up and
reposted the moment a packet is copied out, so nothing on the receive
path allocates. Transmit is a header-plus-payload chain through the
shared `submit_chain`/`await_completion` pair, so several tasks transmit
at once.

Everything above the wire format is device-neutral. The AF_VSOCK value
types and the `VsockDevice` trait live in `hal/src/vsock.rs` — vsock is a
hypervisor transport contract that virtio is one implementation of — and
the connection table, credit accounting and port allocation live in
`kernel/src/vsock/`. Programs reach it through
`helios:system/vsock@0.1.0`.

Its backend is `vhost-vsock`, which QEMU implements only against the host
kernel's `/dev/vhost-vsock`: there is no user-space vsock backend, so on
a host without that device node — every macOS host, and any Linux host
without the `vhost_vsock` module — no guest can have the device at all.
`helios-inspector vm --rpc-transport vsock` therefore checks the host
before it builds anything and refuses with an explanation rather than
booting a guest whose device is silently absent. The default transport
stays the serial line; see `docs/inspector-vsock.md`.

virtio-gpu is the machine's display engine. The driver in
`virtio/src/gpu.rs` drives the two queues the device defines and nothing
else:

| Queue | Index | Commands |
| --- | --- | --- |
| control | 0 | `GET_DISPLAY_INFO`, `GET_EDID`, `RESOURCE_CREATE_2D`, `RESOURCE_ATTACH_BACKING`, `RESOURCE_DETACH_BACKING`, `RESOURCE_UNREF`, `SET_SCANOUT`, `TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH` |
| cursor | 1 | `UPDATE_CURSOR`, `MOVE_CURSOR` |

The split is what the hardware cursor plane is for: pointer motion is one
command on a queue of its own, so it never queues behind a frame's
transfer and costs no pixels at all. Every control response header is
checked against the reply the request asked for, and an `ERR_*` answer
becomes the `DisplayError` variant that names it —
`ERR_INVALID_SCANOUT_ID` becomes `UnknownScanout`, `ERR_OUT_OF_MEMORY`
becomes `OutOfMemory` — rather than a log line and a retry. A code that
belongs to no request this driver issues, `ERR_INVALID_CONTEXT_ID`
included, is reported as `UnexpectedResponse` because a 2D driver never
asked the question it answers.

One class feature is negotiated: `VIRTIO_GPU_F_EDID` (bit 1), so that a
scanout's preferred mode is the attached monitor's own preferred detailed
timing rather than whatever geometry the host last published.
`VIRTIO_GPU_F_VIRGL` (0), `VIRTIO_GPU_F_RESOURCE_UUID` (2),
`VIRTIO_GPU_F_RESOURCE_BLOB` (3) and `VIRTIO_GPU_F_CONTEXT_INIT` (4) are
deliberately never asked for: the 3D path needs host-visible blob memory
mapped into a guest address space plus a fence protocol, which is a
different contract from this one. The configuration space is read whole —
`events_read`/`events_clear` drive the display-change notification,
`num_scanouts` bounds the display-info reply, and `num_capsets` is
reported and otherwise unused, because a capability set describes a 3D
context type.

**The driver never allocates a frame buffer.** `create_framebuffer` is
handed physical ranges the caller already owns and publishes them as the
resource's backing store; the pages stay the caller's, the device only
reads them, and `destroy_framebuffer` detaches the backing before
dropping the resource so the caller's pages are never still on loan. A
driver that allocated the pixels itself would own memory the kernel has
to account for, would tie the frame buffer's lifetime to the device's,
and would put a second allocator where the kernel most wants one. It
follows that a virtio-gpu function behind a translation unit is refused
at bring-up: its caller's pages are not in its domain, and the first
scanout would fetch from an address the unit rejects.

Everything above the wire format is device-neutral. The display value
types and the `DisplayDevice` trait live in `hal/src/display.rs` — a
scanout and a cursor plane are display-engine facts that virtio-gpu is
one implementation of — and the kernel holds the device through
`install_display_device` (`kernel/src/io/display.rs`), whose task
consumes the device's `VIRTIO_GPU_EVENT_DISPLAY` announcements and reads
the new topology back. An announcement nobody collects stays latched and
the next change raises no interrupt at all, which is why the kernel owns
the device from bring-up.

The device is on the platform's own bus: virtio-pci on x86-64
(`-device virtio-gpu-pci`) and virtio-mmio on aarch64 and riscv64
(`-device virtio-gpu-device`). Each backend reports it on one line:

```
virtio-gpu online transport=mmio scanouts=1 preferred=1280x800 edid=on
```

virtio-input is the machine's keyboard, pointer and tablet. The driver
in `virtio/src/input.rs` carries evdev events unchanged — a
`virtio_input_event` is a Linux `input_event` without its timestamp — so
there is no Helios event model to translate through. The codes
themselves are generated from a named revision of Linux's
`include/uapi/linux/input-event-codes.h` into `hal/src/input/codes.rs`
by `tools/gen-input-event-codes.py`; the header is `GPL-2.0-only WITH
Linux-syscall-note`, and the note is what allows a non-GPL tree to carry
the ABI constants. The `input-event-codes` crate on crates.io was not
used: it is plain `GPL-2.0-only` with no such note, and its newest
release describes Linux 6.2.

Two queues serve the device: `eventq` (0), which the device reports on,
and `statusq` (1), which carries indicator changes back to it. The event
ring is the driver's whole receive buffer pool — one eight-byte event per
descriptor, allocated at bring-up and reposted the moment it is read — so
nothing on the receive path allocates. Events are handed to the reader
one at a time, `SYN_REPORT` included: a frame boundary is a fact the
consumer acts on, and a driver that buffered a frame would add latency to
the one path a person can feel.

An input device is also the only device in the tree that reports without
being asked, and that changes what its driver owes the interrupt line.
The reader clears the device's interrupt status before it parks, not only
when an interrupt arrives: virtio-mmio derives its line from that
read-to-clear register, so a status nobody reads holds the line asserted,
and a line that never falls never rises again — an edge-triggered
controller sees no further interrupt and the device is silent for the
life of the machine. The case is not hypothetical. A device that is
started before the platform routes its interrupt raises that line into a
controller which is not yet listening, and on GICv3 the pending state a
level-configured line left behind is discarded when the trigger is
reprogrammed to edge; the reader's first park is what clears the raise
nobody could deliver.

The device describes itself through a select/sub-select configuration
register file rather than a command protocol (virtio 1.2 §5.8.5): the
driver writes `select` and `subsel`, reads back `size`, and then reads
that many payload bytes. `ID_NAME`, `ID_SERIAL` and `ID_DEVIDS` name the
device; `PROP_BITS` says what kind of thing it is; `EV_BITS` answers per
event type, and a non-zero size *is* the declaration that the type is
supported, because the specification defines no query that lists them;
`ABS_INFO` gives each absolute axis its range. The selector is
device-wide state, so the file is read exactly once, on the bring-up
path, by the processor that programs the device, and never afterwards.

Everything above the wire format is device-neutral. The evdev value
types and the `InputDevice` trait live in `hal/src/input.rs`, and the
kernel holds every device through `install_input_devices`
(`kernel/src/input/`), whose task per device drains the ring for as long
as the machine runs. A device nobody reads is a device that stops
working — its ring is its whole buffer pool — which is why the kernel
owns it from bring-up and never hands the device itself anywhere.

What is *done* with the events is the claim's business.
`helios:system/input` hands the right to read one device to exactly one
instance at a time, and the drain relays whole reports into that
instance's stream; a device nobody has claimed has its events written to
the kernel's log instead, which is what makes a machine with no
compositor readable. `docs/desktop.md` describes the interface and the
lane that drives it.

The device is on the platform's own bus: virtio-pci on x86-64
(`-device virtio-keyboard-pci`, `-device virtio-mouse-pci`, `-device
virtio-tablet-pci`) and virtio-mmio on aarch64 and riscv64 (the same
names ending `-device`). A machine presents several at once and every one
of them is brought up, each on its own interrupt; the backends report one
line per device:

```
virtio-input online transport=pci name="QEMU Virtio Tablet" ev=KEY,REL,ABS abs=x:0..32767,y:0..32767
```

virtio-snd is the machine's sound card. The driver in `virtio/src/snd.rs`
plays PCM audio; capture is deliberately not driven, so the receive queue
is programmed and never posted on — a queue the driver puts no buffer on
is a queue the device has nothing to complete, which is how a
capture-capable device is told this driver is not recording. Four queues
serve it (virtio 1.2 §5.14.2):

| Queue | Index | Carries |
| --- | --- | --- |
| control | 0 | `JACK_INFO`, `PCM_INFO`, `CHMAP_INFO`, `PCM_SET_PARAMS`, `PCM_PREPARE`, `PCM_START`, `PCM_STOP`, `PCM_RELEASE` |
| event | 1 | `PCM_PERIOD_ELAPSED`, `PCM_XRUN`, `JACK_CONNECTED`, `JACK_DISCONNECTED` |
| transmit | 2 | One period per chain, on its way to the device |
| receive | 3 | Nothing: programmed and never posted |

A stream is a state machine and the control queue is how it is driven.
`PCM_SET_PARAMS` fixes the format, rate, channel count, buffer and
period; `PCM_PREPARE` makes the device allocate; `PCM_START` begins its
clock; `PCM_STOP` and `PCM_RELEASE` undo the two. Every reply carries a
status word and every one of them is checked against the four the
specification defines — `S_OK`, `S_BAD_MSG`, `S_NOT_SUPP`, `S_IO_ERR` —
and turned into the `AudioError` variant that names it. A code outside
those four is reported as `UnexpectedResponse`, because it answers a
question this driver never asked. The parameters themselves are checked
against the stream's own `PCM_INFO` description first: a format the
stream does not accept, a rate it cannot be clocked at, a channel count
outside its range, a period that does not divide the buffer or does not
end on a frame boundary are all refused here, where the field that is
wrong can still be named, rather than at the device, whose whole answer
would be one `BAD_MSG`.

One period is one transmit chain: a four-byte `virtio_snd_pcm_xfer`
naming the stream, the caller's period, and an eight-byte
`virtio_snd_pcm_status` the device writes back. **The driver never
allocates a period.** The bytes are the caller's, on loan to the device
between the submission and the completion, and the status carries
`latency_bytes` — how much the device still held unplayed when it took
this period — which is the only unit a device can state its latency in.
The transmit ring is 64 chains deep and every chain has its own
completion slot, so a caller with several `write` futures alive at once
has several periods in flight; a device that runs dry between two periods
plays a gap, and the gap is audible.

The event ring is the driver's whole receive buffer pool — one eight-byte
`virtio_snd_event` per descriptor, allocated at bring-up and reposted the
moment it is read — so nothing on the receive path allocates. Like
virtio-input, a sound device reports without being asked, so its reader
clears the device's interrupt status before it parks rather than only
when an interrupt arrives, and the bring-up path clears the interrupt its
own polled query raised: on virtio-mmio the line is a function of a
read-to-clear register, and a line that never falls never rises again.

`VIRTIO_SND_F_CTLS` is deliberately not negotiated: it adds the mixer
control protocol, which is a different contract from this one and has no
consumer in the tree. Neither is any per-stream PCM feature —
`MSG_POLLING` and the shared-memory period features are alternatives to
the event ring this driver reads.

Everything above the wire format is device-neutral. The PCM value types
and the `PlaybackDevice` trait live in `hal/src/audio.rs` — a stream, a
jack and a channel map are sound-hardware facts that virtio-snd is one
implementation of — and the kernel holds the device through
`install_sound_device` (`kernel/src/io/sound.rs`), whose task per device
drains the event ring for as long as the machine runs and logs what it
finds. A device nobody reads is a device that stops reporting, which is
why the kernel owns it from bring-up; the audio service takes playback
over from there. The formats the contract names are the linear ones a
mixer can write straight into a buffer (`S8`, `U8`, `S16`, `U16`, `S32`,
`U32`, `FLOAT`, `FLOAT64`); a device that also offers mu-law, a packed
3-byte width, DSD or IEC958 subframes has that part of its bitmap dropped
rather than refused, because each of those needs a conversion step that
belongs to whatever produces the audio.

The device is on the platform's own bus: virtio-pci on x86-64 (`-device
virtio-sound-pci`) and virtio-mmio on aarch64 and riscv64 (`-device
virtio-sound-device`). `helios-inspector vm --audiodev` is what attaches
it, and `docs/desktop.md` describes the backends. Each backend reports it
on one line:

```
virtio-snd online transport=pci streams=1 jacks=1 rates=5512..192000 formats=S16,S32,FLOAT
```

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
