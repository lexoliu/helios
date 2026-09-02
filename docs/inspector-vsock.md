# Inspector RPC over vsock

The inspector talks to the guest debugger through the WIT RPC defined in
`helios-inspector-protocol`. That protocol needs a byte stream, and the
kernel offers two of them.

## The two transports

`--rpc-transport serial` (the default) frames RPC on the debug serial
line. That line also carries the guest console and the kernel boot log,
so the RPC framing shares a link with everything the kernel prints.

`--rpc-transport vsock` frames RPC on a vsock connection to the guest
debugger, which binds port 1024 (`helios_inspector_protocol::VSOCK_RPC_PORT`)
whenever the machine has a vsock device. The serial line then carries
only the console and the boot log, and the inspector keeps draining and
echoing it for the life of the session.

Boot markers stay on the serial line under both transports: they are
printed before any RPC transport exists, so the serial socket is still
what tells the inspector that the guest reached `wasi:cli/run`. The vsock
connection is opened once it has.

The transport is chosen, never guessed. There is no automatic fallback in
either direction: a session that asks for vsock on a host that cannot
provide it fails with an explanation naming `--rpc-transport serial`.

## What the host has to provide

QEMU has no user-space vsock backend. The only backend is `vhost-vsock`,
which is the host kernel carrying the packets, and it needs
`/dev/vhost-vsock`:

- **Linux**: load the module once with `sudo modprobe vhost_vsock`.
- **macOS**: not available. QEMU on macOS is built without any vsock
  device model at all — `qemu-system-aarch64 -device help` lists neither
  `vhost-vsock-device` nor `vhost-vsock-pci` — so a macOS host runs its
  sessions on the serial transport.

`helios-inspector vm` checks this before it builds or boots anything, so
an unusable request fails immediately rather than as a QEMU device error
several minutes into a kernel build.

## Context ids

Each guest gets a context id (`--vsock-cid`, default derived from the
inspector's own process id, always 3 or greater). Ids 0, 1 and 2 are
reserved for the hypervisor, the retired loopback address, and the host.
The hypervisor rejects a duplicate id, so two concurrent sessions on one
host get different ids and a genuine collision fails QEMU startup loudly
rather than attaching to the wrong machine.

## Running it

```bash
sudo modprobe vhost_vsock
./target/release/helios-inspector vm --arch riscv64 --release --memory 2G \
    --rpc-transport vsock \
    --boot-program dash --boot-program debugger --no-compiler-plugin \
    shell -c 'echo ok'
```

The same selection is available in a VM config file as `rpc_transport`
and `vsock_cid`.
