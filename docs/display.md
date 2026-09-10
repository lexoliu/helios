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

## Where the pixels are

Nowhere the kernel owns.

A surface's frame buffer is pinned, physically contiguous memory
committed from the claiming instance's own pool and placed at a fixed
offset inside that instance's linear memory. Those same pages are what
the display engine is told to read. So:

- A compositor draws a frame with ordinary stores into
  `surface.buffer()`. No call, no copy, no staging buffer.
- `surface.present(region)` is one `TRANSFER_TO_HOST_2D` of the
  rectangle that changed and one `RESOURCE_FLUSH`. The kernel never
  touches a pixel.
- A compositor that asks for a larger surface grows its *own* memory
  accounting. The kernel's does not move.

The pages go in the **display window** — the span immediately below the
device window `docs/device-grants.md` describes, at the top of the
instance's linear-memory reservation, above everything it can grow into.
The instance's growth is capped below the window for as long as it holds
the display, so a `memory.grow` can never land on a frame buffer the
display engine is scanning out. That is the same mechanism a granted
device's registers use, instantiated a second time rather than sharing
one arena, so neither can run into the other.

| Window | Size | What lives there |
| --- | --- | --- |
| Device | `DEVICE_WINDOW_BYTES` (64 MiB) | a granted device's register mappings and DMA rings |
| Display | `DISPLAY_WINDOW_BYTES` (256 MiB) | a claim's frame buffers and its cursor plane |

The display window is sized for the large end of what a compositor asks
a single machine's display engine for: a handful of surfaces at 4K in a
32-bit format, whose frames are 33 MiB each. Reaching the end of it is
answered with `window-exhausted` rather than quietly reusing memory the
display engine may still be reading. A surface's span comes back to the
arena when it is the most recent one — which is what changing mode does
— and otherwise stays held until the claim ends, exactly as a granted
device's pinned rings are held for as long as the grant.

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
instance drops it — and a drop cannot await anything. So the release
does neither of the two obvious things:

1. It does not tell the device. That is asynchronous, and the drop
   raises a signal the control server races against its inbox instead.
2. It does not free the pages. The display engine may still be scanning
   them out. The arena is handed to the control server, which blanks
   every output, takes the pointer off, drops every resource, and only
   then lets the arena go — the point at which the pages are the
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
