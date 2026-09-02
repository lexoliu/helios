//! Virtqueues in both ring layouts.
//!
//! [`VirtQueue`] is the single queue type every driver uses. The ring
//! layout is a device capability discovered during feature negotiation,
//! not a compile-time choice, so the queue wraps a private [`Ring`] enum
//! whose two variants — the split ring of virtio 1.0 and the packed ring
//! of virtio 1.1 — implement the same private [`RingOps`] contract. The
//! public surface is identical either way, so no driver, and no backend
//! that instantiates a driver, is duplicated per layout.
//!
//! Concurrency contract: a `VirtQueue` is a single-owner structure. Every
//! mutating entry point takes `&mut self`, so callers serialise access
//! through whatever lock owns the queue (an async mutex for the request
//! queues, a spin mutex for the per-CPU transmit rings). Nothing inside
//! the queue takes a lock of its own, and completions may be drained by
//! whichever task wins that lock — the queue never assumes the task that
//! submitted a chain is the task that reaps it.

mod packed;
mod split;
#[cfg(test)]
mod tests;

use core::alloc::Layout;

use alloc::boxed::Box;
use helios_hal::io::{IoError, IoResult};

use crate::bus::{DeviceBus, DmaBuffer, DmaPool};
use crate::features::NegotiatedFeatures;
use crate::transport::VirtioTransport;

use packed::PackedRing;
use split::SplitRing;

/// The DMA buffer type a transport's bus hands out.
pub(crate) type TransportBuffer<T> =
    <<<T as VirtioTransport>::Bus as DeviceBus>::DmaPool as DmaPool>::Buffer;

/// Both ring layouts use 16-byte descriptors, so one indirect table
/// stride covers both.
pub(crate) const DESCRIPTOR_BYTES: usize = 16;

/// Shortest chain that is worth pushing into an indirect table. A single
/// buffer already fits in one ring descriptor, so indirecting it would
/// only add a level of device-side indirection.
const INDIRECT_MIN_CHAIN: usize = 2;

/// Upper bound on the buffers one submission may carry, independent of
/// the per-queue limit. Bounds the stack scratch every submission uses.
pub const MAX_CHAIN_BUFFERS: usize = 16;

const NO_ID: u16 = u16::MAX;

/// Failures that come from the queue itself rather than from the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VirtqueueError {
    #[error("virtqueue size {0} is not a non-zero power of two")]
    InvalidSize(u16),
    #[error("virtqueue chain limit {0} exceeds the {MAX_CHAIN_BUFFERS}-buffer submission bound")]
    InvalidChainLimit(u16),
    #[error("a descriptor chain must carry at least one non-empty buffer")]
    EmptyChain,
    #[error("descriptor chain of {actual} buffers exceeds this queue's limit of {limit}")]
    ChainTooLong { actual: usize, limit: usize },
    #[error("virtqueue has no room for a chain of {needed} descriptors")]
    Full { needed: usize },
    #[error("virtqueue ring memory could not be allocated")]
    RingAllocation,
    #[error("this queue did not negotiate VIRTIO_F_RING_RESET")]
    ResetUnsupported,
}

impl From<VirtqueueError> for IoError {
    fn from(error: VirtqueueError) -> Self {
        match error {
            VirtqueueError::InvalidSize(_) | VirtqueueError::InvalidChainLimit(_) => {
                Self::Unsupported
            }
            VirtqueueError::EmptyChain => Self::DeviceFault,
            VirtqueueError::ChainTooLong { .. } => Self::OutOfBounds,
            VirtqueueError::Full { .. } => Self::DeviceFault,
            VirtqueueError::RingAllocation => Self::DeviceFault,
            VirtqueueError::ResetUnsupported => Self::Unsupported,
        }
    }
}

/// One buffer of a descriptor chain, already translated to a device
/// address.
#[derive(Clone, Copy)]
pub(crate) struct ChainEntry {
    addr: u64,
    len: u32,
    writable: bool,
}

/// Free descriptor identifiers, handed out first-in first-out.
///
/// FIFO order is what makes VIRTIO_F_IN_ORDER expressible: the feature
/// requires the driver to consume the descriptor table in ring order,
/// and a queue whose completions arrive in submission order returns its
/// identifiers to the tail in exactly the order they were taken from the
/// head, so the pool stays sequential on its own.
struct IdPool {
    next: Box<[u16]>,
    head: u16,
    tail: u16,
    free: u16,
}

impl IdPool {
    fn new(size: u16) -> Self {
        let mut next: Box<[u16]> = (0..size).map(|index| index + 1).collect();
        next[usize::from(size - 1)] = NO_ID;
        Self {
            next,
            head: 0,
            tail: size - 1,
            free: size,
        }
    }

    fn reset(&mut self) {
        let size = self.next.len();
        for (index, slot) in self.next.iter_mut().enumerate() {
            *slot = u16::try_from(index + 1).unwrap_or(NO_ID);
        }
        self.next[size - 1] = NO_ID;
        self.head = 0;
        self.tail = u16::try_from(size - 1).unwrap_or(NO_ID);
        self.free = u16::try_from(size).unwrap_or(u16::MAX);
    }

    fn available(&self) -> u16 {
        self.free
    }

    /// The identifier the next allocation will hand out.
    fn peek(&self) -> u16 {
        self.head
    }

    fn allocate(&mut self) -> Option<u16> {
        if self.free == 0 {
            return None;
        }
        let id = self.head;
        self.head = self.next[usize::from(id)];
        if self.head == NO_ID {
            self.tail = NO_ID;
        }
        self.free -= 1;
        Some(id)
    }

    fn release(&mut self, id: u16) {
        assert!(
            usize::from(id) < self.next.len(),
            "virtqueue descriptor {id} is outside the pool"
        );
        self.next[usize::from(id)] = NO_ID;
        if self.tail == NO_ID {
            self.head = id;
        } else {
            self.next[usize::from(self.tail)] = id;
        }
        self.tail = id;
        self.free += 1;
    }
}

/// In-flight chain identifiers in submission order.
///
/// Only maintained when VIRTIO_F_IN_ORDER is negotiated, where it is what
/// lets the driver expand a batched completion — one used entry standing
/// for several chains — back into the individual chains.
struct OrderFifo {
    ids: Box<[u16]>,
    head: usize,
    len: usize,
}

impl OrderFifo {
    fn new(size: u16) -> Self {
        Self {
            ids: alloc::vec![0_u16; usize::from(size)].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    fn push(&mut self, id: u16) {
        assert!(
            self.len < self.ids.len(),
            "virtqueue submission order overflow"
        );
        let slot = (self.head + self.len) % self.ids.len();
        self.ids[slot] = id;
        self.len += 1;
    }

    fn front(&self) -> Option<u16> {
        (self.len != 0).then(|| self.ids[self.head])
    }

    fn pop_front(&mut self) -> Option<u16> {
        if self.len == 0 {
            return None;
        }
        let id = self.ids[self.head];
        self.head = (self.head + 1) % self.ids.len();
        self.len -= 1;
        Some(id)
    }

    /// Zero-based distance of `id` from the front, or `None` when the
    /// identifier is not in flight.
    fn position(&self, id: u16) -> Option<usize> {
        (0..self.len).find(|offset| self.ids[(self.head + offset) % self.ids.len()] == id)
    }
}

/// One pre-allocated indirect descriptor table per queue slot.
///
/// The table for chain identifier `id` is always slot `id`, so a
/// submission never has to allocate: the head descriptor points at its
/// own slot and the chain is written there.
struct IndirectTables<B: DmaBuffer> {
    memory: B,
    stride: usize,
    entries: usize,
}

impl<B: DmaBuffer> IndirectTables<B> {
    fn table_ptr(&self, id: u16) -> *mut u8 {
        let offset = usize::from(id) * self.stride;
        assert!(
            offset + self.stride <= self.memory.len(),
            "indirect table {id} is outside the queue's table memory"
        );
        unsafe { self.memory.as_ptr().add(offset) }
    }

    fn table_addr(&self, id: u16) -> u64 {
        self.memory.phys_addr() + (usize::from(id) * self.stride) as u64
    }

    fn entries(&self) -> usize {
        self.entries
    }
}

/// The in-order completion batch currently being handed out one chain at
/// a time.
#[derive(Clone, Copy, Default)]
struct InOrderBatch {
    remaining: u16,
    final_len: u32,
}

/// Per-queue state both ring layouts share.
struct RingCore<B: DmaBuffer> {
    size: u16,
    chain_limit: usize,
    features: NegotiatedFeatures,
    ids: IdPool,
    /// Total writable bytes of each in-flight chain. VIRTIO_F_IN_ORDER
    /// lets a device report a batch of completions with a single used
    /// entry, in which case the spec defines the skipped buffers as
    /// having been used completely — which is exactly this length.
    writable_len: Box<[u32]>,
    order: OrderFifo,
    batch: InOrderBatch,
    indirect: Option<IndirectTables<B>>,
    /// Ring entries published since the last notification decision.
    num_added: u16,
}

impl<B: DmaBuffer> RingCore<B> {
    fn new<P>(
        dma: &P,
        size: u16,
        chain_limit: u16,
        features: NegotiatedFeatures,
    ) -> Result<Self, VirtqueueError>
    where
        P: DmaPool<Buffer = B>,
    {
        let chain_limit = usize::from(chain_limit);
        let indirect = if features.indirect() && chain_limit >= INDIRECT_MIN_CHAIN {
            let stride = chain_limit * DESCRIPTOR_BYTES;
            let layout = Layout::from_size_align(stride * usize::from(size), DESCRIPTOR_BYTES)
                .map_err(|_| VirtqueueError::RingAllocation)?;
            Some(IndirectTables {
                memory: dma
                    .allocate_zeroed(layout)
                    .map_err(|_| VirtqueueError::RingAllocation)?,
                stride,
                entries: chain_limit,
            })
        } else {
            None
        };

        Ok(Self {
            size,
            chain_limit,
            features,
            ids: IdPool::new(size),
            writable_len: alloc::vec![0_u32; usize::from(size)].into_boxed_slice(),
            order: OrderFifo::new(size),
            batch: InOrderBatch::default(),
            indirect,
            num_added: 0,
        })
    }

    fn clear(&mut self) {
        self.ids.reset();
        self.order.clear();
        self.batch = InOrderBatch::default();
        self.num_added = 0;
        self.writable_len.fill(0);
    }

    /// Whether a chain of `buffers` descriptors should be pushed into
    /// this queue's indirect table.
    fn use_indirect(&self, buffers: usize) -> bool {
        self.indirect.is_some() && buffers >= INDIRECT_MIN_CHAIN
    }

    fn check_chain(&self, chain: &[ChainEntry]) -> Result<(), VirtqueueError> {
        if chain.is_empty() {
            return Err(VirtqueueError::EmptyChain);
        }
        if chain.len() > self.chain_limit {
            return Err(VirtqueueError::ChainTooLong {
                actual: chain.len(),
                limit: self.chain_limit,
            });
        }
        Ok(())
    }

    fn record_chain(&mut self, id: u16, chain: &[ChainEntry]) {
        let writable = chain
            .iter()
            .filter(|entry| entry.writable)
            .map(|entry| entry.len)
            .fold(0_u32, u32::saturating_add);
        self.writable_len[usize::from(id)] = writable;
        if self.features.in_order() {
            self.order.push(id);
        }
    }

    /// Opens an in-order completion batch that the device reported with a
    /// single used entry naming `final_id`.
    fn begin_in_order_batch(&mut self, final_id: u16, final_len: u32) {
        let chains = self.order.position(final_id).unwrap_or_else(|| {
            panic!(
                "virtqueue in-order completion named descriptor {final_id}, which is not in flight"
            )
        });
        self.batch = InOrderBatch {
            remaining: u16::try_from(chains + 1).unwrap_or(u16::MAX),
            final_len,
        };
    }

    /// Hands out the next chain of the open in-order batch.
    fn next_in_order_completion(&mut self) -> (u16, u32) {
        let id = self
            .order
            .pop_front()
            .expect("virtqueue in-order batch outlived its submissions");
        self.batch.remaining -= 1;
        let len = if self.batch.remaining == 0 {
            self.batch.final_len
        } else {
            // virtio 1.2 §2.7.9: buffers the device skipped over inside a
            // batch are defined to have been used completely.
            self.writable_len[usize::from(id)]
        };
        (id, len)
    }
}

/// Operations both ring layouts implement.
trait RingOps<T: VirtioTransport>: Sized {
    fn new(
        transport: &T,
        index: u16,
        size: u16,
        chain_limit: u16,
        features: NegotiatedFeatures,
    ) -> Result<Self, VirtqueueError>;

    /// Writes one chain into the ring and returns its identifier.
    ///
    /// `publish` asks for the chain to be made visible to the device
    /// immediately; a deferred submission leaves that to [`RingOps::publish`].
    fn submit(&mut self, chain: &[ChainEntry], publish: bool) -> Result<u16, VirtqueueError>;

    /// Makes every chain submitted since the last publication visible.
    fn publish(&mut self);

    fn pop_used(&mut self) -> Option<(u16, u32)>;

    fn should_notify(&self) -> bool;

    /// The VIRTIO_F_NOTIFICATION_DATA payload for the current ring
    /// position.
    fn notification_data(&self, index: u16) -> u32;

    fn clear_added(&mut self);

    fn available(&self) -> usize;

    fn next_id(&self) -> u16;

    /// Drops every in-flight chain and re-programs the device-side queue
    /// after a [`VirtioTransport::reset_queue`].
    fn reprogram(&mut self, transport: &T, index: u16);
}

/// The ring layout this queue negotiated.
enum Ring<T: VirtioTransport> {
    Split(SplitRing<T>),
    Packed(PackedRing<T>),
}

impl<T: VirtioTransport> Ring<T> {
    fn submit(&mut self, chain: &[ChainEntry], publish: bool) -> Result<u16, VirtqueueError> {
        match self {
            Self::Split(ring) => ring.submit(chain, publish),
            Self::Packed(ring) => ring.submit(chain, publish),
        }
    }

    fn publish(&mut self) {
        match self {
            Self::Split(ring) => ring.publish(),
            Self::Packed(ring) => ring.publish(),
        }
    }

    fn pop_used(&mut self) -> Option<(u16, u32)> {
        match self {
            Self::Split(ring) => ring.pop_used(),
            Self::Packed(ring) => ring.pop_used(),
        }
    }

    fn should_notify(&self) -> bool {
        match self {
            Self::Split(ring) => ring.should_notify(),
            Self::Packed(ring) => ring.should_notify(),
        }
    }

    fn notification_data(&self, index: u16) -> u32 {
        match self {
            Self::Split(ring) => ring.notification_data(index),
            Self::Packed(ring) => ring.notification_data(index),
        }
    }

    fn clear_added(&mut self) {
        match self {
            Self::Split(ring) => ring.clear_added(),
            Self::Packed(ring) => ring.clear_added(),
        }
    }

    fn available(&self) -> usize {
        match self {
            Self::Split(ring) => ring.available(),
            Self::Packed(ring) => ring.available(),
        }
    }

    fn next_id(&self) -> u16 {
        match self {
            Self::Split(ring) => ring.next_id(),
            Self::Packed(ring) => ring.next_id(),
        }
    }

    fn reprogram(&mut self, transport: &T, index: u16) {
        match self {
            Self::Split(ring) => ring.reprogram(transport, index),
            Self::Packed(ring) => ring.reprogram(transport, index),
        }
    }
}

/// A virtqueue in whichever layout the device and driver agreed on.
pub struct VirtQueue<T: VirtioTransport> {
    index: u16,
    features: NegotiatedFeatures,
    ring: Ring<T>,
}

impl<T: VirtioTransport> VirtQueue<T> {
    /// Allocates and programs one virtqueue.
    ///
    /// `chain_limit` is the longest descriptor chain the owning driver
    /// will ever submit on this queue. It bounds the pre-allocated
    /// indirect tables, so a queue that only ever carries single buffers
    /// allocates none at all, and a longer chain is rejected instead of
    /// silently overrunning a table.
    pub fn new(
        transport: &T,
        index: u16,
        size: u16,
        chain_limit: u16,
        features: NegotiatedFeatures,
    ) -> IoResult<Self> {
        if size == 0 || !size.is_power_of_two() {
            return Err(VirtqueueError::InvalidSize(size).into());
        }
        if chain_limit == 0 || usize::from(chain_limit) > MAX_CHAIN_BUFFERS {
            return Err(VirtqueueError::InvalidChainLimit(chain_limit).into());
        }

        let ring = if features.packed() {
            Ring::Packed(PackedRing::new(
                transport,
                index,
                size,
                chain_limit,
                features,
            )?)
        } else {
            Ring::Split(SplitRing::new(
                transport,
                index,
                size,
                chain_limit,
                features,
            )?)
        };

        Ok(Self {
            index,
            features,
            ring,
        })
    }

    /// The features this queue was built with.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    /// Submits one chain of read-only buffers followed by writable ones
    /// and makes it available to the device immediately.
    pub fn submit(
        &mut self,
        transport: &T,
        inputs: &[&[u8]],
        outputs: &mut [&mut [u8]],
    ) -> IoResult<u16> {
        let mut chain = [ChainEntry {
            addr: 0,
            len: 0,
            writable: false,
        }; MAX_CHAIN_BUFFERS];
        let used = build_chain(transport, inputs, outputs, &mut chain)?;
        Ok(self.ring.submit(&chain[..used], true)?)
    }

    /// Stages one read-only buffer without publishing it.
    pub(crate) fn submit_read_only_deferred(
        &mut self,
        transport: &T,
        input: &[u8],
    ) -> IoResult<u16> {
        self.submit_deferred(transport, &[input], &mut [])
    }

    /// Stages a chain of read-only buffers without publishing it.
    pub(crate) fn submit_read_only_chain_deferred(
        &mut self,
        transport: &T,
        parts: &[&[u8]],
    ) -> IoResult<u16> {
        self.submit_deferred(transport, parts, &mut [])
    }

    /// Stages one writable buffer without publishing it.
    pub(crate) fn submit_output_deferred(
        &mut self,
        transport: &T,
        output: &mut [u8],
    ) -> IoResult<u16> {
        self.submit_deferred(transport, &[], &mut [output])
    }

    fn submit_deferred(
        &mut self,
        transport: &T,
        inputs: &[&[u8]],
        outputs: &mut [&mut [u8]],
    ) -> IoResult<u16> {
        let mut chain = [ChainEntry {
            addr: 0,
            len: 0,
            writable: false,
        }; MAX_CHAIN_BUFFERS];
        let used = build_chain(transport, inputs, outputs, &mut chain)?;
        Ok(self.ring.submit(&chain[..used], false)?)
    }

    /// Makes every deferred submission visible to the device.
    pub(crate) fn publish(&mut self) {
        self.ring.publish();
    }

    /// Reaps the next completed chain, if any.
    pub fn pop_used(&mut self) -> Option<u16> {
        self.pop_used_with_len().map(|(id, _)| id)
    }

    /// Reaps the next completed chain together with the number of bytes
    /// the device wrote into it.
    pub fn pop_used_with_len(&mut self) -> Option<(u16, u32)> {
        self.ring.pop_used()
    }

    /// Reaps every completed chain, handing each to `complete`.
    ///
    /// This is the entry point a task uses when it may be reaping work
    /// another task submitted: completions arrive in whatever order the
    /// device chose, and the caller routes each one by its identifier.
    pub fn drain_used(&mut self, mut complete: impl FnMut(u16, u32)) -> usize {
        let mut drained = 0;
        while let Some((id, len)) = self.ring.pop_used() {
            complete(id, len);
            drained += 1;
        }
        drained
    }

    /// Kicks the device if it has not suppressed notifications.
    pub fn notify(&mut self, transport: &T) {
        if self.ring.should_notify() {
            if self.features.notification_data() {
                transport
                    .notify_queue_with_data(self.index, self.ring.notification_data(self.index));
            } else {
                transport.notify_queue(self.index);
            }
        }
        self.ring.clear_added();
    }

    /// Free descriptor slots left in the ring.
    pub fn available_descriptors(&self) -> usize {
        self.ring.available()
    }

    /// The identifier the next submission will be given.
    pub fn next_free_descriptor(&self) -> u16 {
        self.ring.next_id()
    }

    /// Resets this queue on the device side and re-programs it.
    ///
    /// Every chain still in flight is abandoned: after the device-side
    /// reset the device owns none of them, so the identifiers go back to
    /// the pool and the rings start from a clean state. Callers holding
    /// completion tokens must treat them as void afterwards.
    pub fn reset(&mut self, transport: &T) -> IoResult<()> {
        if !self.features.ring_reset() {
            return Err(VirtqueueError::ResetUnsupported.into());
        }
        transport.reset_queue(self.index)?;
        self.ring.reprogram(transport, self.index);
        Ok(())
    }

    /// Releases the queue on the device side and leaves it disabled.
    ///
    /// Drivers call this from their `Drop` because they, not the queue,
    /// own the transport the reset has to go through. Unlike
    /// [`VirtQueue::reset`] the queue is not re-programmed afterwards:
    /// the caller is about to free the ring memory, and re-enabling the
    /// queue would leave the device pointing at it. Without
    /// VIRTIO_F_RING_RESET there is no way to take the queue away from
    /// the device short of resetting the whole device, so the rings stay
    /// device-visible until the owning driver does that.
    pub fn shutdown(&mut self, transport: &T) {
        if !self.features.ring_reset() {
            return;
        }
        if let Err(error) = transport.reset_queue(self.index) {
            tracing::warn!(
                queue = self.index,
                ?error,
                "virtqueue could not be reset on teardown"
            );
        }
    }
}

/// Translates a driver's buffers into device addresses.
fn build_chain<T: VirtioTransport>(
    transport: &T,
    inputs: &[&[u8]],
    outputs: &mut [&mut [u8]],
    chain: &mut [ChainEntry; MAX_CHAIN_BUFFERS],
) -> Result<usize, VirtqueueError> {
    let buffers = inputs.len() + outputs.len();
    if buffers == 0 {
        return Err(VirtqueueError::EmptyChain);
    }
    if buffers > MAX_CHAIN_BUFFERS {
        return Err(VirtqueueError::ChainTooLong {
            actual: buffers,
            limit: MAX_CHAIN_BUFFERS,
        });
    }

    let dma = transport.bus().dma();
    let mut used = 0;
    for input in inputs {
        chain[used] = entry(dma, input, false)?;
        used += 1;
    }
    for output in outputs.iter() {
        chain[used] = entry(dma, output, true)?;
        used += 1;
    }
    Ok(used)
}

fn entry<P: DmaPool>(dma: &P, buffer: &[u8], writable: bool) -> Result<ChainEntry, VirtqueueError> {
    if buffer.is_empty() {
        return Err(VirtqueueError::EmptyChain);
    }
    Ok(ChainEntry {
        addr: dma
            .dma_addr(buffer.as_ptr())
            .map_err(|_| VirtqueueError::RingAllocation)?,
        len: u32::try_from(buffer.len()).map_err(|_| VirtqueueError::ChainTooLong {
            actual: buffer.len(),
            limit: u32::MAX as usize,
        })?,
        writable,
    })
}

/// `vring_need_event` (virtio 1.2 §2.7.6.1): kick only when the device's
/// advertised event index falls inside the window of entries published
/// since the previous notification decision.
fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}
