# Guest networking under the inspector

`helios-inspector vm` attaches exactly one virtio-net device to the
guest. Which host-side packet path sits behind that device is selected
with `--net-backend`, and it decides what the guest's virtio-net driver
can negotiate at all: multiqueue, TCP segmentation offload and checksum
offload — in both directions — are host-path properties, not guest
choices. A benchmark taken
over the wrong backend measures QEMU's slirp copy loop rather than the
driver.

The inspector never silently substitutes one backend for another. Every
combination a backend cannot satisfy — multiqueue on slirp, a tap on
macOS, an interface name on a backend with no host interface — is a typed
error naming the reason.

## What each backend can exercise

| Backend | Host | Multiqueue | TSO (`HOST_TSO4/6`) | Checksum (`CSUM`) | Receive offload (`GUEST_CSUM`, `GUEST_TSO4/6`) | Packed ring | Privilege |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `user` | any | no (1 pair) | no | no | no | yes | none |
| `tap` | Linux | yes (`--net-queues`) | yes | yes | yes | yes | one-time `net-setup` |
| `vmnet-shared` | macOS | no (1 pair) | yes | yes | yes | yes | root or entitlement |
| `vmnet-bridged` | macOS | no (1 pair) | yes | yes | yes | yes | root or entitlement |
| `socket-vmnet` | macOS | no (1 pair) | yes | yes | yes | yes | daemon installed once |

The packed-ring column is a property of the virtqueue rather than the
backend: `--virtio-packed` and `--virtio-in-order` are applied to every
virtio device the inspector creates, including this one, so any backend
can be measured on either ring layout.

`tap` is the only backend that can serve more than one queue pair.
`--net-queues` therefore defaults to `--smp` on `tap` and to 1
everywhere else; asking any other backend for more than one pair is an
error rather than a quiet downgrade.

## Reading back what was negotiated

Two `info` lines per device land in the boot log. The generic
`virtio features negotiated` line (from `virtio/src/features.rs`) reports
the ring bits, and `virtio-net online` (from `virtio/src/net.rs`) reports
the device-class facts, transmit and receive:

```
INFO [helios_virtio::net] virtio-net online queue_pairs=4 csum=true
  host_tso4=true host_tso6=true guest_csum=true guest_tso4=true
  guest_tso6=true guest_ecn=true guest_ufo=true mrg_rxbuf=true mq=true
  ctrl_vq=true rss=true hash_report=true notf_coal=true vq_notf_coal=true
  notf_coal_max_packets=8 notf_coal_max_usecs=50 status=true link_up=true
  max_frame_len=1514
  max_receive_frame_len=65550 rx_buffer_len=4096
```

The `guest_*` bits are the receive half: `guest_csum` is what lets the
device tell the stack, per frame, that it validated the transport
checksum or left it partial, and `guest_tso4`/`guest_tso6`/`guest_ufo`
let it hand over one frame coalesced out of several wire segments.
Those need somewhere to put a 64 KiB frame, which is `mrg_rxbuf`:
receive buffers are one page each and a coalesced frame arrives as a
chain of them, so `max_receive_frame_len` exceeds `max_frame_len`
exactly when receive segmentation was negotiated.

`rss` and `hash_report` are the steering bits. With
`VIRTIO_NET_F_RSS` the device hashes each received frame's four-tuple,
masks the hash to a slot in a 128-entry indirection table the driver
programmed, and delivers the frame on the queue that slot names — so a
flow arrives on the queue whose processor already owns its socket, with
no cross-core hand-off. The table is `slot % queue_pairs` and the key is
the standard 40-byte Toeplitz key, which is also what the kernel's own
demux computes: a device that cannot steer delivers everything on queue
zero and the software hash still routes the flow to the same shard, so
only the CPU hop differs. `hash_report` adds the device's hash to the
receive header (and eight bytes to the header in both directions) so the
kernel reads that number instead of computing it again.

A device that offers RSS but cannot hold a 128-entry table, a 40-byte
key, or all four of the TCP/UDP over IPv4/IPv6 hash types is left
unsteered rather than half-programmed — a wrong table would put a flow
on a queue whose processor does not own the socket, which is worse than
the extra hop. The boot log says so when it happens.

`notf_coal` and `vq_notf_coal` are the notification-coalescing bits
(`VIRTIO_NET_F_NOTF_COAL` / `VIRTIO_NET_F_VQ_NOTF_COAL`): the device
holds a queue's notification back until `notf_coal_max_packets` frames
have accumulated or `notf_coal_max_usecs` have passed, whichever comes
first, so a saturated queue raises one interrupt per batch instead of
one per descriptor and an idle one is still bounded by the delay. With
`vq_notf_coal` the budget is programmed per virtqueue, which is what a
per-CPU queue layout wants — each pair is driven by its own processor at
its own rate, and one device-wide setting would make an idle pair pay
the busy pair's delay. Without it the same budget is set once for every
receive queue and once for every transmit queue.

`helios-inspector vm` echoes every guest boot line it reads before the
debugger comes up as `guest serial: …` on stderr, so capturing the
inspector's stderr is enough to record which offloads a run actually had.

## Link state

With `VIRTIO_NET_F_STATUS` the device reports carrier, and a change
arrives as a configuration-change interrupt rather than a queue
notification. The driver re-reads the status word, publishes it, and the
kernel network service drops the addresses, routes, neighbours and
resolvers that describe the link that went away; when carrier returns it
puts every shard's DHCP client and router solicitation back at the start
so the new link is configured from scratch.

QEMU can drive that from the HMP monitor the inspector already exposes,
which is the cheapest way to exercise the path on any backend:

```bash
helios-inspector vm --arch aarch64 --release \
    --monitor unix:/tmp/helios-hmp.sock,server=on,wait=off tracing &
printf 'set_link net0 off\n' | nc -U /tmp/helios-hmp.sock
printf 'set_link net0 on\n' | nc -U /tmp/helios-hmp.sock
```

The streamed guest tracing shows the driver's
`virtio-net link state changed` followed by the service's
`network link down` / `network link up` lines.

## Capturing the wire

`--net-pcap <path>` attaches QEMU's `filter-dump` to the guest's netdev
and writes every frame crossing it, both directions, to a pcap file. It
works on every backend, because the filter sits between the virtio-net
device and the host packet path, so what lands in the file is what the
guest driver actually sent and received rather than what the host end
saw:

```bash
helios-inspector vm --arch aarch64 --release \
    --net-pcap /tmp/helios-net.pcap \
    workload-bench --workload tcp-throughput --iterations 1
```

Frames are captured up to 65550 bytes, so a receive-segmentation-coalesced
frame is recorded whole rather than truncated at the point that would hide
what the driver negotiated. This is the first thing to reach for when a
transfer stalls: whether the guest stopped acknowledging, whether the peer
stopped sending, and which of the two was waiting on a timer are all
questions the capture answers and the guest log does not.

## The `user` backend

The default. QEMU's built-in slirp stack needs no privileges and no host
state, reaches the host at `10.0.2.2`, and answers DHCP itself, so the
guest's own DHCP client leases an address without anything else running
on the host. It emulates one queue pair inside the QEMU process and
offers no offload, which makes it the right choice for functional work
and the wrong one for any measurement of the network path.

## The `tap` backend (Linux)

A multi-queue tap device enslaved to a host bridge, driven by
`vhost=on` so the packet copy runs in host kernel threads rather than
the QEMU main loop. The tap outlives individual VMs: it is provisioned
once by the privileged helper and the VM command never elevates by
itself.

```bash
# once per host, prints every privileged command before running it
helios-inspector vm net-setup \
    --net-backend tap --net-ifname helios0 --net-bridge helios-br0 --net-dhcp

# per run
helios-inspector vm --arch aarch64 --release \
    --net-backend tap --net-ifname helios0 --net-bridge helios-br0 \
    --net-queues "$(nproc)" \
    workload-bench --workload tcp-throughput

# when the host state is no longer wanted
helios-inspector vm net-teardown \
    --net-backend tap --net-ifname helios0 --net-bridge helios-br0
```

`net-setup` runs under `sudo` unless it is already root, prints each
command as it would be retyped, and supports `--dry-run` to print the
plan without touching the host. The plan is:

1. `ip link add <bridge> type bridge`, `ip link set <bridge> up`
2. `ip addr replace <address> dev <bridge>` — `--net-bridge-address`,
   default `10.77.0.1/24`. This address is what the guest reaches the
   host on, replacing slirp's `10.0.2.2`.
3. `ip tuntap add dev <ifname> mode tap multi_queue user <uid> group <gid>`
   — `multi_queue` is the whole point; without `IFF_MULTI_QUEUE` the tap
   caps the guest at one queue pair no matter what `--net-queues` says.
   The device is owned by the invoking user (`SUDO_UID`/`SUDO_GID` when
   the helper itself is run under `sudo`) so an unprivileged QEMU can
   open it.
4. `ip link set <ifname> master <bridge>`, `ip link set <ifname> up`
5. unless `--net-no-nat`: `sysctl -w net.ipv4.ip_forward=1` and an
   nftables `helios-nat` table with a masquerade rule out of
   `--net-uplink` (defaulting to the interface owning the host default
   route, read from `ip -json route show default`).
6. with `--net-dhcp`: a `dnsmasq` bound to the bridge alone, serving the
   subnet's host range above the bridge address.

After the plan runs, the helper re-reads
`/sys/class/net/<ifname>/tun_flags` and fails unless `IFF_MULTI_QUEUE`
(`0x0100`) is set: a tap that silently came up single-queue would turn
every later multiqueue measurement into a measurement of one queue.

`helios-inspector vm --net-backend tap` performs the same checks before
building anything — the interface exists, it is multi-queue when more
than one queue pair was asked for, and, when `--net-bridge` is given, it
is enslaved to that bridge — so a missing tap costs seconds instead of a
kernel rebuild.

### Guest addressing: DHCP stays in the guest

The kernel's own DHCP client (`kernel/src/network/service`) is the single
addressing path, on every backend. slirp answers DHCP itself; a bare
bridge does not, so the tap backend needs a responder on the host and
`net-setup --net-dhcp` starts a `dnsmasq` bound to the bridge for exactly
that. It is opt-in rather than mandatory because a host that already runs
a DHCP server on that bridge, or a lane that only needs host↔guest
traffic to a statically known address, should not have a second one
started behind its back.

The alternative — passing a static address into the guest through
`-fw_cfg` and short-circuiting the DHCP client — was rejected: it would
give the tap backend an addressing path no other backend uses, and the
DHCP client is itself part of what these lanes are meant to exercise.

`dnsmasq` is therefore a host requirement of `--net-dhcp` (Debian and
Ubuntu ship it as `dnsmasq-base`); without the flag, the guest will not
lease an address on a `tap` backend and the boot log will show its DHCP
client retrying.

### Hosts with a default-DROP forward policy

The masquerade rule only covers NAT. A host whose `iptables` `FORWARD`
policy is `DROP` — which is what Docker installs — also needs the bridge
allowed through:

```bash
sudo iptables -I FORWARD -i helios-br0 -j ACCEPT
sudo iptables -I FORWARD -o helios-br0 -j ACCEPT
```

This is deliberately not part of `net-setup`: mixing `nft` and
`iptables` rules into a host's existing firewall is the kind of change
that should be made knowingly. Host↔guest traffic to the bridge address
does not traverse `FORWARD` and works without it, so the benchmark lanes
do not depend on this.

## The `vmnet` backends (macOS)

`vmnet-shared` puts the guest on a NATed vmnet interface; `vmnet-bridged`
bridges it onto a host interface named with `--net-ifname` (`en0`, …).
Both are single queue: the vmnet framework exposes one packet path per
interface. Both need either root or a QEMU binary carrying the
`com.apple.vm.networking` entitlement, and the inspector checks for both
with `codesign` before it builds anything rather than letting QEMU fail
after the fact.

## The `socket-vmnet` backend (macOS)

[`socket_vmnet`](https://github.com/lima-vm/socket_vmnet) is a root
daemon that opens the vmnet interface once and hands unprivileged
processes a connected unix socket. QEMU is exec'd under its client
launcher, which passes the socket as file descriptor 3:

```bash
helios-inspector vm --arch aarch64 --net-backend socket-vmnet
```

`--socket-vmnet-path` names the daemon endpoint (default
`/opt/socket_vmnet/var/run/socket_vmnet`) and `--socket-vmnet-client`
names the launcher (default `socket_vmnet_client`, resolved through
`PATH`). Installing and starting the daemon is a one-time launchd setup
documented upstream; the inspector verifies the endpoint is a live unix
socket and the launcher is executable, and fails typed otherwise.

## Tuning the device itself

`--net-device-prop key=value` forwards a virtio-net device property to
QEMU. The name is validated against the properties the inspector knows,
so a typo is reported before the VM is created rather than by QEMU
halfway through machine construction:

| Property | Values | Effect |
| --- | --- | --- |
| `csum` | `on`/`off` | Offer `VIRTIO_NET_F_CSUM` (guest may hand the host partial checksums). |
| `guest_csum` | `on`/`off` | Offer `VIRTIO_NET_F_GUEST_CSUM`. |
| `host_tso4` | `on`/`off` | Offer IPv4 TCP segmentation offload. |
| `host_tso6` | `on`/`off` | Offer IPv6 TCP segmentation offload. |
| `mrg_rxbuf` | `on`/`off` | Offer mergeable receive buffers. |
| `event_idx` | `on`/`off` | Offer `VIRTIO_F_RING_EVENT_IDX` on this device. |
| `indirect_desc` | `on`/`off` | Offer `VIRTIO_F_INDIRECT_DESC` on this device. |
| `rss` | `on`/`off` | Offer `VIRTIO_NET_F_RSS` (steer received flows across queues). |
| `hash` | `on`/`off` | Offer `VIRTIO_NET_F_HASH_REPORT` (report the flow hash in the receive header). |
| `notf_coal` | `on`/`off` | Offer `VIRTIO_NET_F_NOTF_COAL` (device-wide notification coalescing). |
| `vq_notf_coal` | `on`/`off` | Offer `VIRTIO_NET_F_VQ_NOTF_COAL` (per-virtqueue coalescing). |

Turning an offload off is how its contribution is measured:

```bash
helios-inspector vm --arch aarch64 --release \
    --net-backend tap --net-ifname helios0 --net-queues 8 \
    --net-device-prop host_tso4=off --net-device-prop host_tso6=off \
    workload-bench --workload tcp-throughput
```

Three property names are recognised but rejected, because the inspector
derives them and a second source would make the resulting device
ambiguous: `mq` and `vectors` come from `--net-queues`, and `packed`
comes from `--virtio-packed`. The error names the flag to use instead.

## Continuous integration

The `bench` job's x86-64 Linux lane provisions a tap through `net-setup`
and runs with `--net-backend tap --net-queues $(nproc)` (the tap netdev
always carries `vhost=on`), so multiqueue and offload are exercised on
every run and the negotiated feature set is printed into the job log.
**That lane is the multi-queue network baseline**, and until an Arm
runner with KVM exists it is the only one CI has.

It is also the only `bench` lane. Every helios CI job runs on a Linux
runner, and no GitHub-hosted runner can produce an aarch64 baseline of
any kind. The Arm Linux runners expose neither `/dev/kvm` nor a
readable `/dev/vhost-net`, so a guest there runs under TCG behind a
userspace tap: no accelerator, and none of the multiqueue or offload
paths the tap backend exists to exercise. The macOS lane that used to
stand in for them was no better — its run record showed
`"accel":["tcg"]`, because GitHub's macOS runners report
`kern.hv_support=0` and the inspector's capability probe falls through
to TCG (#118). A lane in either shape measures the emulator, so the
arm64 baseline, CPU-side and multi-queue network alike, is taken on a
real arm64 machine or a self-hosted runner (see AGENTS.md §3.5) rather
than read out of CI.

The one aarch64 lane CI does run, `smoke-aarch64`, is a functional
check on `ubuntu-24.04-arm` under TCG and not a performance surface. It
boots the guest from a device tree and from ACPI against a pinned
upstream QEMU that the lane builds and caches, because the QEMU Ubuntu
24.04 ships asserts in its emulated GICv3 CPU interface under
multi-threaded TCG (#85).
