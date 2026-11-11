# The display: `helios:system/display`

The kernel owns the machine's display device and hands the *right to
draw on it* to exactly one instance at a time. Those are two different
things, and the split is the whole design.

The kernel has to own the device. A monitor plugged in, unplugged, or
resized by whatever hosts this machine announces itself through a
configuration-change interrupt, and an announcement nobody consumes
leaves the event latched — after which the *next* change raises no
interrupt at all. So a kernel task follows the device's topology for as
long as the machine runs, whether or not anybody is drawing.

What a compositor gets is a claim. `display.claim` hands it out, one
holder at a time, and dropping it — or dying while holding it — releases
every resource and leaves the outputs blank before anybody else is
offered the display. That is the same rule `device.claim` follows
(`docs/device-grants.md`), for the same reason: a compositor waiting for
a display another compositor holds is a provisioning mistake, not a
shortage.

Holding the display is a right, not a race. An instance may claim it
only when its `ProcessAuthority` carries the display device class, which
the kernel grants to the compositor it starts and to the debugger, and
which a program receives only from a parent that holds it. Every other
program is told what a machine with no display tells it: `unavailable`.

## Where the pixels are

Nowhere the kernel owns.

A surface's frame buffer is two objects. The **buffer** is a pinned,
physically contiguous run of frames charged to the claiming instance's
own pool; it has no address of its own, and it is what the display
engine is told to read. The **view** is that buffer appearing in the
instance's linear memory, at pages the instance chose, below
`memory.size`. So:

- A compositor draws a frame with ordinary stores, `memory.copy` and
  `memory.fill` into the view. No call, no copy, no staging buffer.
  Every byte it touches is inside its own memory by the runtime's own
  account, so a scalar access and a bulk one are bounded identically.
- `surface.present(region)` is one `TRANSFER_TO_HOST_2D` of the
  rectangle that changed and one `RESOURCE_FLUSH`. The kernel never
  touches a pixel.
- A compositor that asks for a larger surface grows its *own* memory
  accounting. The kernel's does not move.

The instance picks the pages. `display.create(scanout, mode, format,
buffer)` takes a `span` — `offset` and `length`, both whole 64 KiB wasm
pages of the caller's own memory — and the length it needs is
`width * height * 4` rounded up to whole pages. The guest's allocator is
the only party that knows which of its pages are free, so
`helios-api` takes a page-aligned block from the program's heap, passes
its span, and holds the block and the kernel's surface in one value:
`Surface::pixels` is a slice of the program's own heap, and the block is
freed only after the surface has been released. A span that is not whole
pages, reaches past `memory.size`, has the wrong length or overlaps a
view already shown is refused with `bad-span`.

The kernel substitutes the backing of the span in one step, from the
task that runs the instance, and only for an unshared memory, so nothing
else reads the span while it changes. What the pages held before is
discarded and the frame starts zeroed. When the view ends the span is
ordinary zeroed memory again. There is no per-instance reservation, no
fixed region to run out of and no growth cap: a surface costs the pages
the program's allocator gave it and the frames charged to its pool.

## The pointer is the display engine's

`set-cursor(image, hotspot)` writes the cursor plane's own 64 by 64
resource and publishes it. `move-cursor(x, y)` moves the plane and does
nothing else: no pixels, no frame-buffer traffic, no repaint. That is
the whole reason a cursor plane exists, and it is why the kernel serves
it on the display engine's own cursor queue with its own task — pointer
motion never waits behind a frame somebody is presenting.

## How a request reaches the device

The display device is a backend type — a virtio-gpu resource behind
whichever transport the platform exposes it on — and the component host
that serves the WIT interface never names it. What crosses that boundary
is a queue, not a trait object:

```
compositor's store            kernel/src/display
──────────────────            ──────────────────
display.create      ──┐
surface.present     ──┼──▶ control queue ──▶ control server ──▶ device
surface.set-cursor  ──┘                          (one task)
surface.move-cursor ─────▶ cursor queue  ──▶ cursor server  ──▶ device
                                                 (one task)
                           release signal ──▶ control server
                                              (blank, destroy, unpin)
```

Three tasks, all local to the processor the device's interrupt is routed
to: the topology follower, the control server and the cursor server. The
control server serves one request at a time, so frames on one display
are published in the order they were asked for. Both queues are bounded:
a compositor that has queued `REQUEST_QUEUE_DEPTH` frames the display
engine has not kept up with waits for room, which is the backpressure a
display path needs.

## Letting go

A claim is let go by a *drop* — the store's, when whatever killed the
instance drops it — and a drop cannot await anything. The view and the
buffer behind it end at different moments, because they have different
holders:

1. The view belongs to the store and ends with it, synchronously,
   before the runtime resets the instance's memory slot. The span is
   ordinary zeroed memory again, and the slot is reusable at once.
2. The buffer does not end there. The display engine may still be
   scanning it out, so the drop does not tell the device — that is
   asynchronous — and does not free the frames. It raises a signal the
   control server races against its inbox. The control server blanks
   every output, takes the pointer off, drops every resource, and only
   then lets the buffer go — the point at which the frames are the
   instance's pool's again.

Between those two moments the display is neither held nor free: the
claim word reads `RELEASING`, and a claim that arrives then is refused
rather than handed a screen with somebody else's pixels still on it.

## Evidence

`programs/display-test` claims the display, creates a surface at the
output's preferred mode, fills it with a gradient whose colour at every
pixel is a function of that pixel's position, presents it, and then
walks the hardware cursor around while re-presenting on a fixed cadence.

The gradient is the point. A display nothing ever attached a resource to
also produces a valid PNG, so "the file is a PNG" says nothing;
`tools/desktop/check-gradient.py` recomputes the expected colour from
the capture's own dimensions and checks it at the corners and the middle.

```bash
./target/release/helios-inspector vm --arch x86-64 --release --accel kvm \
    --desktop --display none \
    --boot-program dash --boot-program debugger --boot-program display-test \
    screendump \
      --run /bin/display-test --run-arg --seconds --run-arg 20 \
      --settle-seconds 2 target/probe/drawn.png
python3 tools/desktop/check-gradient.py target/probe/drawn.png
```

`smoke-x86-64` runs exactly that and uploads the captures as the
`smoke-x86-64-desktop` artifact.

One thing the capture cannot show: the **hardware cursor plane is not in
it**. QEMU delivers a virtio-gpu cursor to its display frontend rather
than compositing it into the scanout surface `screendump` reads, and a
headless session has no frontend. What the pointer path is proved by
instead is the driver's own fake-transport tests — a `move-cursor` costs
one cursor-queue command and publishes no control chain at all — and
`display-test`'s own log line for every position it put the pointer at.
A guest that *follows* an injected pointer position needs the
virtio-input driver, which is a separate contract.
