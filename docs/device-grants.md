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

## Status

Phase 1 landed the `hal` capability types, `kernel/src/device/`, the
`helios:system/device@0.1.0` contract, and the hosted machine's device
and tests.

Two things are open, and both are recorded on #5:

* **The kernel-side implementation of `helios:system/device` is not
  wired.** Placing a mapping inside an instance's linear memory needs
  the base address and length of that component instance's core memory.
  The runtime exposes this for core modules (`Instance::get_memory`,
  which the preview1 path uses) but not for components: a component's
  `get_export` cannot return a memory, and a component host function
  receives only a `StoreContextMut`. The interface is declared and
  imported by no world until that primitive exists.
* **Bare-metal discovery is not built.** `hosted/` is the only grant
  source. PCI enumeration on x86-64 and device tree nodes on aarch64 and
  riscv64 are the remaining half of phase 1's backend work.
