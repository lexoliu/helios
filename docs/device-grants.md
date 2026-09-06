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
register: it maps the device's own frames *inside* the instance's
linear-memory reservation, and `map-region` hands back the byte offset
they landed at. From the driver's side a register is `*(base + offset)`.

The mappings go in a **device window** at the top of the reservation:

```
0                                          window offset        reservation end
|-- the instance's linear memory --------- | -- device window -- | -- guard --
                                             regions, then pinned buffers
```

`DEVICE_WINDOW_BYTES` is 64 MiB and the window sits at the very top of
the four-gigabyte reservation every instance gets, so the memory it
displaces is memory a `wasm32` instance could never address anyway.
`DeviceWindow::offset` is both where the window starts and the highest
the instance's memory may grow to: whoever builds the window is
responsible for capping the instance's growth limit there, so a
`memory.grow` can never land on a register file.

Regions and buffers are carved out of the window by a bump cursor at the
address space's own mapping granule, which is what
`DeviceVmHooks::mapping_granule` reports — the host page on `hosted/`,
and never smaller than a frame. Carving finer than the granule would put
two regions in one page, and changing either would change both.

A region that does not start and end on a frame boundary is refused at
grant construction. The page it shares with its neighbour would carry
the neighbour's registers into the owner's memory, and the neighbour may
be a device nobody granted away.

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

`dma-alloc` pins a physically contiguous run inside the device window,
from the *owner's* pool, and reports both the linear-memory offset the
driver fills it through and the address the device has to issue to reach
it. The two are different numbers: the device address is a physical
address on a machine with no translation unit in the path and an I/O
virtual address on one that confines the device, and `hal`'s
`DmaTranslation` is what turns one into the other.

Two bounds apply. The grant carries a `DmaBudget` — a policy the kernel
sets, so a driver cannot squeeze every other instance out of the user
pool — and a `DmaCapability`, which is a hardware fact: a device that
drives 32 address bits cannot reach a buffer above 4 GiB, so the kernel
allocates under its limit rather than discovering the truncation as
corruption.

A buffer lives as long as the grant. A driver builds its rings once,
during its own bring-up; there is no per-request pin, and a driver that
wants one is a driver that should be reusing a ring.

### Reclaim is what makes the restart cheap

Dropping a lease — explicitly, or because the instance was killed —
masks every source, unmaps every region and releases every pinned buffer
*before* the device is offered to anyone else. Every mapping change goes
through the address space, which invalidates the local translation cache
and shoots down every other processor that has run in the space before
it returns, so the dead owner has provably lost its last path to the
registers by the time the replacement starts.

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
* `AddressSpace::map_device`, `unmap_device` and `commit_contiguous` —
  the three primitives an owner outside the kernel needs.

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

The backend tracks device mappings beside its reservation tracker
rather than inside it. The tracker exists to return frames to the user
pool when a reservation is released, and neither kind of device mapping
may go there: a register window was never taken from the pool, and a
pinned run is owed its own layout. Keeping them separate is also what
makes `release` total — a store torn down by an OOM kill still has its
device mapped, and the address space is the last place that can
guarantee the hardware is unreachable afterwards.

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
