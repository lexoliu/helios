# The desktop devices and how a session drives them

A guest that draws needs a display adapter, a keyboard and two pointers,
and a host that wants to prove what it drew needs to read the scanout
back. `helios-inspector vm` attaches the devices and reads the scanout
through QEMU's machine protocol, so the evidence is a PNG a lane can keep
rather than a window somebody watched.

## The devices

`--desktop` attaches, on whichever transport the architecture's profile
uses for its other virtio devices:

| Device | PCI machine (x86-64) | MMIO machine (aarch64, riscv64) |
| --- | --- | --- |
| Display | `virtio-gpu-pci` | `virtio-gpu-device` |
| Keyboard | `virtio-keyboard-pci` | `virtio-keyboard-device` |
| Absolute pointer | `virtio-tablet-pci` | `virtio-tablet-device` |
| Relative pointer | `virtio-mouse-pci` | `virtio-mouse-device` |

Both pointers arrive because they are different devices rather than two
modes of one: an absolute position moves the tablet, a relative offset
moves the mouse, and a guest given only one of them ignores half of an
input script.

On the PCI machine the same step passes `-vga none`. The machine would
otherwise create a VGA adapter of its own, and QEMU's console 0 — the one
`screendump` captures — would be that adapter's rather than the guest's.
A capture of a display the guest never drove looks exactly like one the
guest failed to draw into, which is the one thing a capture must not do.

Without `--desktop` nothing changes: the machine is the machine every
existing lane boots.

## `--display`

`--display <none|cocoa|gtk|sdl>` is the host display backend QEMU opens.
The default is `none`, which is what every lane boots and what the
capture recipe below uses: the guest still has its display device, and
QEMU still holds its surface, so `screendump` returns exactly what a
window would have shown.

A backend this host's QEMU was not built with is refused by QEMU, naming
itself. Nothing substitutes another one — the same rule the accelerator
follows.

## `--audiodev`

`--audiodev <none|wav:<path>|<host backend>>` names the host audio
backend the guest's sound device plays into. Anything but `none`
attaches `virtio-sound-pci` (or `virtio-sound-device`) against it;
`none`, the default, attaches no sound device at all, because QEMU
refuses a virtio-sound device whose `audiodev` names nothing.

`wav:<path>` writes the guest's playback to a file and needs no host
audio at all, so it is the sink a headless runner records with. Any other
value is a backend of this host's own — `coreaudio`, `pa`, `alsa`,
`dbus` — and QEMU is the one that refuses a name its build does not have.

## Capturing a scanout

`screendump` writes the machine's current scanout as a PNG. It speaks to
QEMU, not to the guest, so it needs a monitor socket; the action asks for
one on its own, and `--keep-runtime-dir` or an explicit
`--qmp unix:<path>,server=on,wait=off` also provides it.

```bash
./target/release/helios-inspector vm --arch x86-64 --release --accel kvm \
    --desktop --display none \
    screendump target/probe/desktop.png
```

Repeat the path to take several captures in a row, and pass
`--settle-seconds <n>` to let the guest draw before each one.

## Driving the desktop

`input <script>` runs a script against the guest's keyboard and pointers.
The script is one statement per line, `#` starts a comment, and every
token becomes a typed value before anything is sent — so a script with a
typo in its last line fails before its first event, rather than leaving
the guest half-driven:

```text
abs 16384 16384      # move the tablet to the middle of the screen
btn left down
btn left up
key ret              # press and release one key
rel 40 -20           # nudge the relative pointer
```

| Statement | What it sends |
| --- | --- |
| `key <qcode>` | The key goes down, then comes back up. Names are QEMU's own: `a`, `ret`, `spc`, `kp_enter`, `shift_r`. |
| `abs <x> <y>` | One absolute position on both axes, in QEMU's `0..=32767` axis range rather than in guest pixels, which the host cannot know. |
| `rel <dx> <dy>` | One relative move on both axes. |
| `btn <left\|right\|middle> <down\|up>` | One button transition. |

Both axes of a move go to the guest in one batch, so a move lands as a
single position rather than as two; a keystroke's press and release are
two batches, so the guest has an interval to observe the key held down
in. `--interval-ms <n>` waits between statements when the guest redraws
between them.

```bash
./target/release/helios-inspector vm --arch x86-64 --release --accel kvm \
    --desktop input target/probe/desktop.input
```

## What the capture proves today

`--desktop` is what the kernel's virtio-GPU driver finds a device on, and
the guest says so on the serial line:

```text
virtio-gpu online transport=mmio scanouts=1 preferred=1280x800 edid=on
```

A guest that has claimed the display through `helios:system/display`
draws into a frame buffer of its own, and the capture is that frame
buffer: `docs/display.md` describes the interface and
`programs/display-test` is the program the lane runs. A session that
boots nothing which draws still produces an image — QEMU's blank scanout,
640x480, carrying QEMU's own "Display output is not active."
placeholder — which is the evidence that the machine had a display at
all.

## Capturing a guest that is drawing

One `vm` session runs one action, and a capture of a guest that is
drawing needs the guest to be drawing at the time. `screendump` therefore
takes the two things that would otherwise need a second boot:

| Option | What it does |
| --- | --- |
| `--run <guest path>` | Starts a program in the guest and leaves it running while the captures are taken. |
| `--run-arg <arg>` | One argument for `--run`. Repeat for several, in order. |
| `--run-wait-seconds <n>` | After the last capture, how long to wait for that program to finish so what it printed reaches this session's output. A program still running when the wait ends is left running. |
| `--input <script>` | An input script, same grammar as the `input` action, run once the program has started and before the first capture. |
| `--input-interval-ms <n>` | How long to wait between that script's statements. |

A guest program that exits before the captures are taken fails the
session, naming itself: a capture is of a guest that is still drawing,
and one taken after the drawing stopped is a capture of whatever was
left.

```bash
./target/release/helios-inspector vm --arch x86-64 --release --accel kvm \
    --desktop --display none \
    --boot-program dash --boot-program debugger --boot-program display-test \
    screendump \
      --run /bin/display-test --run-arg --seconds --run-arg 20 \
      --input tools/desktop/display-probe.input --input-interval-ms 100 \
      --settle-seconds 2 target/probe/drawn.png
```

`smoke-x86-64` runs that on every push and uploads the captures as the
`smoke-x86-64-desktop` artifact, checking the pixels with
`tools/desktop/check-gradient.py` rather than only the file type.
