# The debug serial line

Every Helios guest under `helios-inspector vm` gets one 16550 bound to a
Unix socket in the VM's runtime directory. It carries three things at
once: the kernel console mirror, the embedded debugger's `[KDBG …]`
stage markers, and — under `--rpc-transport serial`, the default — the
inspector RPC itself. `docs/inspector-vsock.md` covers moving the RPC
off it.

## The raw capture

The line is bound as a named chardev, and the chardev keeps a log:

```
-chardev socket,id=helios-debug-serial,path=<runtime>/debug.sock,\
         server=on,wait=on,logfile=<runtime>/debug-serial.log,logappend=off
-serial chardev:helios-debug-serial
```

`debug-serial.log` is a byte-for-byte copy of the line, written by QEMU
as it accepts each byte, before anything on the host frames it.
`--debug-serial-log <PATH>` puts it somewhere else; the runtime
directory is otherwise where it lands, so `--keep-runtime-dir` (and
every CI lane, which already keeps its runtime directories) collects it
with the QEMU log and the guest console.

The capture exists because a byte can be lost in two places and the
guest cannot tell them apart — its transmit succeeds either way:

- **Below the host.** QEMU's 16550 model hands each byte to the chardev
  and, when the host socket is not writable, re-arms a writability
  watch a bounded number of times (`MAX_XMIT_RETRY`) before discarding
  the byte outright. Nothing is logged and nothing is reported. This is
  what a host that stops draining the socket costs.
- **In the reader.** The inspector frames the line into guest lines and
  parses the stage markers out of them.

A marker already broken in `debug-serial.log` was lost below the host; a
marker whole there and broken in the inspector's output was lost by the
reader. That is the whole point of the file: the two failures look
identical in the inspector's log and nowhere else.

## Reading a capture

```bash
python3 tools/debug-serial-report.py <runtime-dir-or-capture> ...
```

It prints each capture's stage markers in order and exits non-zero when
one of them is broken. The kernel writes a marker as `\n[KDBG <stage>]\n`
under the console gate (`kernel/src/io/serial.rs`), so in a whole
capture every marker sits on a line of its own. A marker sharing its
line with another, or one that never closes before the newline, is
bytes the guest wrote and the host never received.

Passing a directory walks it, which is how a lane checks every VM it
booted in one step. `smoke-x86-64` and both `bench` lanes run it.

## Keeping the line drained

Byte loss below the host is a host-side defect, so the inspector's side
of the socket is written to make it impossible:

- the debug serial is read in whole chunks, never a byte per wakeup
  (`SerialLines`, `inspector/src/ready.rs`), and framing a line is a
  scan of bytes already taken;
- the console echo renders on its own thread, so a write to the
  inspector's stderr — a pipe in every CI lane — is never time the
  socket is not being read.

Anything added between one read of this socket and the next has to be
bounded and non-blocking. The line has no flow control: what the host
does not take, QEMU throws away.
