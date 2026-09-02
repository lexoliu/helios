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
