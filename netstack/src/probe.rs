//! TEMPORARY tcp-latency hop probe for issue #354. Not for commit.
//!
//! A fixed ring of raw records written with relaxed atomics: `mark`
//! costs a fetch-add and four stores, so it can sit inside interrupt
//! handlers, the executor loop, and the socket path without touching
//! the console UART (whose per-byte MMIO exits would dominate a
//! 400us round trip). `dump` renders the ring through `tracing` once,
//! after the run, when nobody is measuring.
//!
//! One clock domain: `set_clock` is called by the backend at boot with
//! a fn returning monotonic nanoseconds (aarch64: cntvct/cntfrq). Every
//! site stamps through `now_nanos()` — sites inside `netstack` (which
//! must stay host-test linkable) and `virtio` carry no clock of their
//! own. `cpu` is a call-site argument for the same reason: stack code
//! runs in host tests where `helios_current_processor` is undefined, so
//! those sites pass `CPU_NONE`.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Hop identifiers. The names are stable strings so the dump is
/// self-describing; keep `HOP_NAMES` index-aligned.
pub mod hop {
    pub const GUEST_WRITE: u8 = 1; // guest write() bytes hit the channel stream
    pub const BRIDGE_WRITE_WAKE: u8 = 2; // tcp write-bridge task resumed with bytes
    pub const SOCK_WRITE: u8 = 3; // execute_tcp_write_all_bytes entered
    pub const SOCK_QUEUED: u8 = 4; // bytes queued on the socket's send buffer
    pub const SEG_OUT: u8 = 5; // netstack queued an outbound TCP segment
    pub const VTX_SUBMIT: u8 = 6; // virtio TX ring submit begin
    pub const VTX_KICK: u8 = 7; // virtio TX notify (kick) returned
    pub const VTX_USED: u8 = 8; // virtio TX used completions drained
    pub const IRQ_ENTER: u8 = 9; // aarch64 IRQ handler per acknowledged intid
    pub const NET_IRQ: u8 = 10; // virtio-net device handler entered
    pub const IRQ_ACK: u8 = 11; // virtio interrupt status read (MMIO ack done)
    pub const IRQ_PAIR: u8 = 12; // queue pair found with pending completions
    pub const IRQ_WAKE: u8 = 13; // queue-owner wake issued (a=owner, b=foreign)
    pub const NET_IRQ_DONE: u8 = 14; // virtio-net device handler returned
    pub const WAIT_DONE: u8 = 15; // wait_for_shard_progress resolved (a=reason)
    pub const RX_POP: u8 = 16; // a used RX descriptor popped (a=pair b=len)
    pub const SHARD_RX: u8 = 17; // frame delivered into a shard stack
    pub const SEG_IN: u8 = 18; // netstack delivered a TCP segment to a socket
    pub const SHARD_SIG: u8 = 19; // shard arrival signal raised (b=foreign wake)
    pub const BRIDGE_DATA: u8 = 20; // tcp read bridge got socket bytes
    pub const BRIDGE_CHAN: u8 = 21; // read bridge pushed bytes to guest channel
    pub const GUEST_READY: u8 = 22; // guest input pollable resolved with bytes
    pub const GUEST_READ: u8 = 23; // guest read() returned bytes
    pub const PARK: u8 = 24; // cpu decided to park (about to wfi or flag path)
    pub const UNPARK: u8 = 25; // cpu left park (b=1 flag short-circuit)
    pub const WAKE_CPU: u8 = 26; // cross-processor wake issued (a=target)
    pub const TASK_RUN: u8 = 27; // executor polled one runnable (a=0 local,1 global)
    pub const READ_PARK: u8 = 28; // tcp read parked on shard progress
    pub const READ_YIELD: u8 = 29; // receive path yielded before parking
    pub const WRITE_PARK: u8 = 30; // tcp write parked on send-window progress
    pub const PUMP_POLL: u8 = 31; // packet pump poll progressed (a=rx,tx,recl)
    pub const RX_REPOST: u8 = 32; // rx buffers reposted + device kicked (a=pair)
    pub const TIMER_IRQ: u8 = 33; // virtual timer PPI taken
    pub const SPURIOUS_IRQ: u8 = 34; // net irq raised with no completions pending
    pub const TX_POLL: u8 = 35; // reclaim_transmit_completions ran (a=drained)
    pub const NET_POLL: u8 = 36; // poll_network_once result (a=src b=rx,tx,recl)
    pub const READ_DATA: u8 = 37; // poll_tcp_read returned data (a=len)
    pub const WRITE_WAIT_DONE: u8 = 38; // tcp write wait resolved
    pub const GUEST_WRITE_PARK: u8 = 39; // output stream flush_pending parked on room
    pub const IRQ_ROUTE: u8 = 40; // device interrupt route dispatched (a=intid)
    pub const WRITE_FLUSHED: u8 = 41; // drive_tcp from write path submitted frames
    pub const BRIDGE_READ_WAIT: u8 = 42; // bridge read entered socket read (pre-park)
    pub const ANCHOR: u8 = 43; // dump header record
    pub const WAKE_SGI: u8 = 44; // wake SGI taken (a=cpu)
    pub const TX_DEFER: u8 = 45; // tx submit call left frames behind (a=pair b=0 locked, 1 no-room)
}

/// `cpu` value for sites that cannot name a processor (netstack's
/// protocol code, which must stay linkable in host-side tests where
/// `helios_current_processor` is undefined).
pub const CPU_NONE: u8 = u8::MAX;

const HOP_NAMES: &[&str] = &[
    "",
    "guest_write",
    "bridge_write_wake",
    "sock_write",
    "sock_queued",
    "seg_out",
    "vtx_submit",
    "vtx_kick",
    "vtx_used",
    "irq_enter",
    "net_irq",
    "irq_ack",
    "irq_pair",
    "irq_wake",
    "net_irq_done",
    "wait_done",
    "rx_pop",
    "shard_rx",
    "seg_in",
    "shard_sig",
    "bridge_data",
    "bridge_chan",
    "guest_ready",
    "guest_read",
    "park",
    "unpark",
    "wake_cpu",
    "task_run",
    "read_park",
    "read_yield",
    "write_park",
    "pump_poll",
    "rx_repost",
    "timer_irq",
    "spurious_irq",
    "tx_poll",
    "net_poll",
    "read_data",
    "write_wait_done",
    "guest_write_park",
    "irq_route",
    "write_flushed",
    "bridge_read_wait",
    "anchor",
    "wake_sgi",
    "tx_defer",
];

const CAPACITY: usize = 16384;

static COUNT: AtomicUsize = AtomicUsize::new(0);
static T: [AtomicU64; CAPACITY] = [const { AtomicU64::new(0) }; CAPACITY];
static TAG: [AtomicU64; CAPACITY] = [const { AtomicU64::new(0) }; CAPACITY]; // hop << 8 | cpu
static A: [AtomicU64; CAPACITY] = [const { AtomicU64::new(0) }; CAPACITY];
static B: [AtomicU64; CAPACITY] = [const { AtomicU64::new(0) }; CAPACITY];

/// The backend's monotonic nanosecond clock, installed at boot. Stored
/// as a usize because `AtomicPtr<fn>` cannot hold a bare fn pointer.
static CLOCK: AtomicUsize = AtomicUsize::new(0);

/// Installs the clock `now_nanos` stamps with. Called once by the
/// backend during boot, before any probe site can run.
pub fn set_clock(clock: fn() -> u64) {
    CLOCK.store(clock as usize, Ordering::Release);
}

/// Monotonic nanoseconds in the backend's domain; zero before the
/// backend installs its clock (boot-time marks get filtered in post).
#[inline]
pub fn now_nanos() -> u64 {
    let clock = CLOCK.load(Ordering::Acquire);
    if clock == 0 {
        return 0;
    }
    // SAFETY: `clock` was stored from a `fn() -> u64` and never
    // changed to another value afterwards.
    unsafe { core::mem::transmute::<usize, fn() -> u64>(clock)() }
}

/// Records one hop. `t` should come from [`now_nanos`] so every record
/// shares one clock domain.
#[inline]
pub fn mark(t: u64, cpu: u8, hop: u8, a: u64, b: u64) {
    let idx = COUNT.fetch_add(1, Ordering::Relaxed);
    if idx >= CAPACITY {
        return;
    }
    T[idx].store(t, Ordering::Relaxed);
    A[idx].store(a, Ordering::Relaxed);
    B[idx].store(b, Ordering::Relaxed);
    TAG[idx].store((hop as u64) << 8 | cpu as u64, Ordering::Release);
}

/// Renders every recorded hop through `tracing` (UART-bound, slow —
/// call only when the measurement is over) and resets the ring.
pub fn dump() {
    let total = COUNT.swap(0, Ordering::AcqRel).min(CAPACITY);
    if total == 0 {
        return;
    }
    tracing::info!(
        target: "helios_probe",
        "probe records={total} t_domain=mach_ns"
    );
    for idx in 0..total {
        let tag = TAG[idx].load(Ordering::Acquire);
        let hop = (tag >> 8) as u8;
        let cpu = (tag & 0xff) as u8;
        let name = HOP_NAMES
            .get(hop as usize)
            .copied()
            .unwrap_or("unknown");
        tracing::info!(
            target: "helios_probe",
            "p t={} c={} h={} a={} b={}",
            T[idx].load(Ordering::Relaxed),
            cpu,
            name,
            A[idx].load(Ordering::Relaxed),
            B[idx].load(Ordering::Relaxed),
        );
    }
}
