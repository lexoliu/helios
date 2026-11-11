# Device grants: handing hardware to a user-mode driver

Helios drives virtio itself, in the kernel, and that is not changing.
Hardware outside the virtio ecosystem is an experimental direction, and
a driver for it must not be able to take the kernel down: a
bit-flipped register write or a bad descriptor ring should kill one
instance and cost one restart, not panic the machine and not make the
next boot harder to debug.

A **device grant** is how that is arranged. The kernel discovers a
device, bundles everything the device *is* into one value, and hands
that value to exactly one user-mode instance. The instance runs under
the ordinary isolation model — the same sandbox, the same memory
accounting, the same supervisor and restart cost as the compiler and
`http-client` plugins — and the kernel keeps the ability to take the
device back.

This page states the contract. Phase 1 of #5 built the kernel side and
the hosted machine's device; the driver-class interfaces that route a
plugin's block, net or serial export back into the kernel are phase 2.

## What a driver needs, and how each part is served

| It needs | It gets | Where |
| --- | --- | --- |
| The device's registers | its physical frames, mapped inside the instance's own linear memory | `GrantLease::map_region` |
| Its interrupts | a stream of deliveries, with the source held off until the driver says otherwise | `InterruptRelay` |
| Bus-mastering memory | a pinned, physically contiguous buffer, and the address the device has to issue for it | `GrantLease::dma_alloc` |
| To be killable | every one of the above undone before anyone else is offered the device | `Drop for GrantLease` |

### Registers are memory, not calls

A register access has to cost what a load costs. Helios owns the user
address space, so the kernel does not have to offer a host call per
register: it shows the device's own frames *inside* the instance's
linear memory, at pages the instance chose, and `map-region(index, at)`
takes the `span` — whole 64 KiB wasm pages of the caller's own memory,
below `memory.size` — they are to appear at. From the driver's side a
register is `*(base + offset)`, with `base` the start of a block the
driver took from its own allocator.

The span is the region's size rounded up to whole wasm pages. The
kernel substitutes the backing of those pages with the region's frames
in one step (`AddressSpace::substitute_device`), carrying the region's
own memory attributes, and only for an unshared memory, so nothing else
reads the span while it changes. A span that is not whole pages, reaches
past `memory.size`, has the wrong length or overlaps a view already
shown is refused with `bad-span`. A region is shown once: asking again
is `already-mapped`. The view lasts as long as the grant; when it ends
the span is ordinary zeroed memory again and the driver may free it.
The pages are the driver's own, and the driver frees them only after
the view has ended; freeing them earlier would hand a device's
registers to its allocator.

The wasm page is larger than any frame, so a region that is smaller than
the span leaves the rest of the span the driver's own memory, and the
region itself is shown from the span's first byte.

A region that does not start and end on a frame boundary is refused at
grant construction. The page it shares with its neighbour would carry
the neighbour's registers into the owner's memory, and the neighbour may
be a device nobody granted away.

A device can also *publish* a region rather than have one granted. The
display engine's host-visible aperture — where a 3D blob's host storage
is placed so the renderer and the guest share it — is the same kind of
view: `blob.map-blob(at)` shows the `DeviceRegion` the device named at
the `span` the claiming instance gave, at `memory-kind = normal`, charges
nothing against the instance's pool because the bytes are the device's,
and ends the view on `unmap-blob` or when the claim is released. What
changes is only who the memory belongs to.

### Interrupts are masked before they are forwarded

The kernel-side handler does the least it can: hold the source off at
the controller, record that it fired, wake the owner. Every decision
about what the device meant runs in user memory.

Masking on delivery is not a policy choice, it is what makes the path
bounded. A level-triggered device keeps its line asserted until its
driver clears the condition in a register, and that driver is a wasm
instance that has not been scheduled yet; leaving the source enabled
would re-enter the handler as fast as the controller can deliver it and
nothing else would ever run. Masking also bounds the pending set by
construction: at most one delivery per source can be outstanding, so a
relay sized to one event per source can never fail to queue one.

The driver therefore sees three calls where a kernel driver sees one:

* `ack(index)` — "I have read whatever the device had to say." A further
  assertion is a new event. This does **not** unmask.
* `unmask(index)` — "I am ready to be interrupted again."
* `mask(index)` — hold it off explicitly.

An event carries a `sequence`, which is the number of deliveries of that
source the kernel has forwarded. A gap between two events a driver sees
is coalescing it can measure rather than infer: the device re-asserted
before the driver acknowledged.

A source is masked when a grant is published, so a device nobody owns
raises nothing, and every source is masked again when an owner dies.

### DMA buffers are the owner's memory

`dma-alloc(align, at)` pins a physically contiguous run from the
*owner's* pool, as long as the span `at` and aligned to `align`, shows
it there zeroed (`AddressSpace::substitute_memory`), and reports the
address the device has to issue to reach it. The driver fills the buffer
through its own span; `dma-buffer` carries only `physical-address`. The
two are different numbers: the device address is a physical address on a
machine with no translation unit in the path and an I/O virtual address
on one that confines the device, and `hal`'s `DmaTranslation` is what
turns one into the other.

Two bounds apply. The grant carries a `DmaBudget` — a policy the kernel
sets, so a driver cannot squeeze every other instance out of the user
pool — and a `DmaCapability`, which is a hardware fact: a device that
drives 32 address bits cannot reach a buffer above 4 GiB, so the kernel
allocates under its limit rather than discovering the truncation as
corruption.

A buffer is shown at its span for as long as the grant lives. A driver
builds its rings once, during its own bring-up; there is no per-request
pin, and a driver that wants one is a driver that should be reusing a
ring.

### Reclaim is what makes the restart cheap

A driver letting go ends every view of a region or buffer
(`GrantLease::end_views`) and then drops the lease; a killed instance
skips the first step, because its view table withdraws every view when
the store dies, synchronously, before the runtime resets the instance's
memory slot, so the slot is reusable at once. Dropping the lease then
masks every source, drops the device's hold on every pinned buffer and
returns the device to the registry *before* it is offered to anyone
else. A buffer whose view is somehow still live keeps its run until that
view ends. What the drop does not do is stop the device's bus mastering:
a device left running by a killed driver can still write into buffers
whose frames have gone back to the pool, which is tracked in #635.
Every mapping change goes through the address
space, which invalidates the local translation cache and shoots down
every other processor that has run in the space before it returns, so
the dead owner has provably lost its last path to the registers by the
time the replacement starts.

If the address space refuses to undo a mapping the kernel panics. It
cannot then prove the device is unreachable, and continuing would hand
that path to whoever the next owner is.

## What the sandbox buys, and what it does not

Memory faults are confined and the restart is cheap. The **bus** is
confined only when the platform has a translation unit and discovery
recorded the device's `IommuDomain`. Without one, a driver that programs
a bad descriptor reaches all of memory — the sandbox isolates the
driver's own faults, not its DMA. The grant says which of the two it is
(`grant.confined()`), rather than implying an isolation the hardware
does not provide.

## Layering

`hal` says what the hardware *is* and names no owner:

* `DeviceRegion` — a physical range plus the rules for reaching it:
  register file versus ordinary memory, writable, prefetchable.
* `DmaCapability` — how many address bits a bus master drives, whether
  its traffic is coherent, and how its addresses are translated.
* `IommuDomain` — the confinement a device sits in.
* `AddressSpace::substitute_device`, `substitute_memory`, `restore` and
  `withdraw` — show a device region or a pinned run at a range of an
  instance's own memory, and put ordinary memory back, which is what an
  owner outside the kernel needs.

`kernel/src/device/` says what the kernel does with it: `DeviceGrant`,
the registry that gives a device to exactly one owner, the relay, the
lease. Backends contribute discovery and two write-once function-pointer
tables (`DeviceVmHooks`, `DeviceInterruptHooks`) — the same shape and
for the same reason as `SwapVmHooks`: there is one address space and one
interrupt controller per machine, chosen at link time, and a driver
should not find a vtable between itself and its registers. No backend
contributes driver logic.

## The hosted device

`hosted/` has no bus to walk and no controller to program, but the
kernel's device path is hardware-independent. The hosted backend
publishes one device, `hosted:device0`, whose registers are an ordinary
host allocation backed by an unlinked temporary file and mapped shared.

The file backing is what makes them a *device* rather than a copy:
file-backed pages can appear at a second address, so the kernel mapping
them into an owner's memory produces a real alias — a write through the
owner's mapping is visible through the backend's, exactly as a register
write is visible to hardware. An anonymous mapping could not do that,
and copying would test nothing.

`hosted/src/device_tests.rs` drives the whole path against it: the alias
both ways, reclaim taking the owner's path away, one owner at a time, a
pinned buffer addressable from both ends and released on death, the
budget, and an interrupt reaching an owner that is already parked.

## Naming

A device is named by the platform's own path to it — `pci:0000:00:04.0`,
a device tree node path, `hosted:device0`. Names are compared, never
parsed: the kernel matches the name a driver asks for against the name
discovery published and interprets neither.

## Device-tree discovery

A device-tree machine describes far more than the kernel drives. The
aarch64 backend's walk takes every node that has a register window and
raises an interrupt, and drops the ones the kernel drives itself — the
interrupt controller, the console UART, the real-time clock, and every
`virtio,mmio` transport. What is left is, by definition, hardware
nobody in the kernel claims, which is exactly what a driver plugin
exists for. On QEMU's aarch64 `virt` board that is the PL061 GPIO
controller; its riscv64 `virt` board describes nothing the kernel does
not already drive, and publishes no grant.

A node whose window is not frame-aligned is skipped with a warning
rather than refused. It is a device this backend cannot isolate, not a
machine it cannot boot: mapping it would put a neighbour's registers in
the same page, and changing one mapping would change both.

Each grant's interrupt is routed at the controller — at the GIC
distributor with the trigger mode the tree declared, or at the PLIC
with a priority — and then left masked. Nothing owns the device
yet, so a line arriving before a claim would have nowhere to go; the
first `unmask` from the driver that claims it is what arms the
hardware.

The DMA capability comes from the firmware — a node that declares no
`dma-ranges` masters the processor's own physical address space, and
`dma-coherent` says whether its accesses snoop. The *budget* does not:
no firmware description says how much memory a driver deserves, because
that is a question about the machine's other tenants. It is kernel
policy, `DEFAULT_DMA_BUDGET_BYTES`, set in one place.

An ACPI-described machine publishes no grants yet. The AML walk looks
for virtio transports by hardware id; naming every other device in the
namespace and reading each one's `_CRS` is a second enumeration, and
the description says what it knows rather than guessing.

## Contiguous memory is one allocation

A device that masters the bus sees physical addresses and no page
table, so a DMA buffer has to be one physical run rather than a list of
frames. The user pool's buddy allocator already answers in contiguous
blocks, so the run is exactly what a single allocation returns — and
that is why releasing one takes the alignment it was made with rather
than being torn down frame by frame. A run given back as frames lands
on the wrong free list.

`DmaPlacement` carries the alignment and the device's address limit
together, because both come from the device and neither means anything
without the other: a commit made against one and checked against the
other is how a buffer ends up somewhere the hardware silently
truncates.

A substituted range is not the reservation tracker's to free. The
tracker exists to return frames to the user pool when a reservation is
released, and neither a device region nor a pinned run belongs there: a
register file was never taken from the pool, and a pinned run is owed
its own layout and is freed by whoever allocated it. Ending a view
(`restore`, or `withdraw` for a store that is dying) never frees what
had been shown. A store torn down by an OOM kill withdraws every view
when its view table drops, before the runtime resets the memory slot,
so by the time the reservation is released nothing is shown in it and
`release` meets only ordinary uncommitted pages; the shootdown each
`withdraw` finishes before it returns is what guarantees the hardware is
unreachable from the dead store.

## Seeing them

A granted device is an ordinary part of the machine's inventory, not a
hidden one, so `helios-inspector stats` lists them beside the
instances: the name discovery published, whether an instance holds it,
how much register space the grant covers, how many lines it raises, and
the forwarded-to-masked counts. A device whose masked count stays equal
to its line count is a driver that has stopped servicing its hardware.

## Status

Phase 1 landed the `hal` capability types, `kernel/src/device/`, the
`helios:system/device@0.1.0` contract and its host implementation, the
hosted machine's device and tests, aarch64 device-tree discovery, and
the inspector listing.

Placing a mapping inside a component instance's linear memory needs the
base address of that instance's core memory. The runtime exposed this
for core modules only, so the vendored fork gained
`wasmtime::component::Instance::get_default_memory`; `docs/wasmtime.md`
records the revision.

### RISC-V has no memory type in its page tables

Whether an access is cacheable and reorderable is a physical memory
attribute of the address on RISC-V, fixed by the platform, rather than
something a leaf PTE selects. (Svpbmt adds one; QEMU's `virt` does not
offer it.) So on riscv64 a register window is an ordinary valid leaf
pointing into the platform's I/O space, and a region's `kind` is
already true of the address it names. On aarch64 the same region takes
MAIR index 7, Device-nGnRnE, because there the page table is what
decides.

### What is open

**x86-64 publishes no grants.** PCI is a different enumeration, not a
port of the device-tree walk: sizing a memory BAR means writing all
ones to it and reading back with the function's memory-space decode
turned off, and masking a granted line means reaching that function's
own MSI-X vector-control bit rather than a controller the kernel owns.
Both deserve their own design and their own review rather than a
tail-end of this one. It is recorded on #5.

What x86-64 does install is the memory half: a display frame buffer is
pinned and shown in an instance's memory on this backend exactly as on
the other two, while `substitute_device` answers
`DeviceMappingUnsupported` because no grant ever names a region here.
