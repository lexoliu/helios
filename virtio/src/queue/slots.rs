//! Device-reachable memory for the small bytes a request carries around
//! its payload: per-descriptor control slots, and a pool of fixed slots
//! the device fills on its own.
//!
//! A request header, a status byte or a short command body used to live
//! in the request's future, or on a stack, and the descriptor named that
//! memory directly. Neither is memory the device owns: a future sits in
//! whatever the executor stored it in, and a request dropped while its
//! chain is in flight frees it under the device. A [`ControlSlots`] table
//! is carved from the device's [`DmaMemory`] once, when the queue is
//! built, with one slot per descriptor identifier. A driver copies the
//! few bytes in before it submits and copies the reply out after the
//! completion; the descriptor names the slot, never the driver's local.
//!
//! Concurrency contract.
//!
//! - Slot `id` belongs to the chain whose head descriptor is `id`, and to
//!   nobody else, from the moment that chain is submitted until the
//!   request that submitted it lets the slot go. The queue never gives out
//!   an identifier that is in flight, and the completion table
//!   ([`InFlight`](crate::inflight::InFlight)) never reports an identifier
//!   idle while its chain is in flight, while its completion is
//!   uncollected, or while a reply is still being read, so "the slot is
//!   free" and "its identifier is idle" are the same fact. That identity is
//!   what stands in for a lock on the slot, and why there are exactly as
//!   many slots as descriptors: a slot cannot run out before a descriptor
//!   does.
//! - The submitting task writes the request area of slot `id`. It does so
//!   while it holds the queue lock, after the completion table said `id`
//!   idle and before the chain is published, so nothing else reads or
//!   writes the slot and the device cannot see it yet.
//! - From publication to completion the device owns the slot: it reads the
//!   request area and writes the response area. The driver touches neither.
//! - A completion may be drained by any task, including from the interrupt
//!   path. Draining records the completion and wakes the waiter; it never
//!   touches slot memory, so it needs no access to this table.
//! - After the completion, the waiter that owns `id` reads the response
//!   area through its [`Reply`](crate::inflight::Reply). The slot stays
//!   held until that reply is dropped, so a later request cannot reuse the
//!   slot while its reply is being read.
//! - A request dropped while its chain is in flight keeps the slot held:
//!   the table lives in the device object, and the identifier is released
//!   by the completion, not by the dropped future. The device completing
//!   late writes into slot memory that is still the slot's.
//!
//! No slot access takes a lock, allocates, or scans: the slot is
//! `base + id * stride`, and the added work per request is the two copies.
//!
//! [`SlotPool`] serves the buffers a device fills without being asked,
//! the receive and event rings. They are not per-descriptor: the driver
//! picks the slot, remembers which identifier it was posted under, and
//! owns the whole pool through `&mut`.

use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::align_of;
use core::ops::Range;

use crossbeam_utils::CachePadded;
use helios_hal::dma::{DmaBuffer, DmaMemory};
use helios_hal::io::{IoError, IoResult};
use helios_hal::iommu::PhysicalRange;

use super::{
    ChainEntry, MAX_CHAIN_BUFFERS, TransportBuffer, VirtQueue, VirtqueueError, entry, foreign_entry,
};
use crate::bus::DeviceBus;
use crate::transport::VirtioTransport;

/// Slots start on a cache line of the target, so two slots written by
/// different processors never share one.
const SLOT_ALIGNMENT: usize = align_of::<CachePadded<u8>>();

/// One buffer of a descriptor chain, as a driver describes it.
///
/// Readable parts come before writable ones, which is the order virtio
/// requires; a chain that breaks it is refused.
pub(crate) enum Part<'a> {
    /// Bytes copied into the slot's request area, which the device reads.
    Request(&'a [u8]),
    /// Memory the caller owns and the device reads.
    Read(&'a [u8]),
    /// A run of somebody else's pages, named by physical address, that the
    /// device reads.
    Payload(PhysicalRange),
    /// Memory the caller owns and the device writes.
    Write(&'a mut [u8]),
    /// Bytes `range` of buffer `buffer` of what the request lent the
    /// device ([`Lend`]), which the device reads.
    LentRead { buffer: usize, range: Range<usize> },
    /// Bytes `range` of buffer `buffer` of what the request lent the
    /// device, which the device writes.
    LentWrite { buffer: usize, range: Range<usize> },
    /// This many bytes of the slot's response area, which the device
    /// writes and the driver copies out after the completion.
    Response(usize),
}

/// The buffers a request moves into the device for as long as its chain
/// is in flight.
///
/// A request lends them when it submits, and they park with the chain's
/// head descriptor in the completion table
/// ([`InFlight`](crate::inflight::InFlight)): the completed request takes
/// them back with its reply, and a request dropped mid-flight leaves them
/// there for the device's completion to free. The chain is described from
/// them before they move into the table, which is sound because every
/// implementation holds [`DmaBuffer`]s, whose bytes do not move with the
/// value.
pub(crate) trait Lend: Send + 'static {
    /// Lent buffer `index`, which [`Part::LentRead`] and
    /// [`Part::LentWrite`] name. `None` when there is no such buffer.
    fn lent(&mut self, index: usize) -> Option<&mut [u8]>;
}

/// A request that lends the device nothing: its data is copied through
/// control slots, or it has none.
impl Lend for () {
    fn lent(&mut self, _index: usize) -> Option<&mut [u8]> {
        None
    }
}

/// A request that may carry one data buffer.
impl<B: DmaBuffer + AsMut<[u8]>> Lend for Option<B> {
    fn lent(&mut self, index: usize) -> Option<&mut [u8]> {
        match (index, self) {
            (0, Some(buffer)) => Some(buffer.as_mut()),
            _ => None,
        }
    }
}

/// A request that carries `K` data buffers.
impl<B: DmaBuffer + AsMut<[u8]>, const K: usize> Lend for [B; K] {
    fn lent(&mut self, index: usize) -> Option<&mut [u8]> {
        self.get_mut(index).map(AsMut::as_mut)
    }
}

/// Bytes `range` of lent buffer `buffer`, or the error that names what the
/// chain asked for and what was lent.
fn lent_bytes<'a, L: Lend>(
    lent: &'a mut L,
    buffer: usize,
    range: &Range<usize>,
) -> Result<&'a [u8], VirtqueueError> {
    let bytes: &'a [u8] = lent.lent(buffer).ok_or(VirtqueueError::LentOutOfRange {
        buffer,
        end: range.end,
        len: 0,
    })?;
    let len = bytes.len();
    bytes
        .get(range.clone())
        .ok_or(VirtqueueError::LentOutOfRange {
            buffer,
            end: range.end,
            len,
        })
}

/// Where a chain's response bytes sit inside its slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResponseSpan {
    offset: usize,
    len: usize,
}

/// One slot per descriptor identifier of one queue.
pub(crate) struct ControlSlots<B: DmaBuffer> {
    /// `None` when the queue never carries slot bytes.
    buffer: Option<B>,
    count: usize,
    /// Capacity of the request area; the response area follows it.
    request: usize,
    response: usize,
    stride: usize,
}

/// The slots of the queue `queue`: one per descriptor identifier, each
/// with `request` bytes the device reads followed by `response` bytes it
/// writes.
///
/// Sized from the queue itself, so a slot exists for every identifier the
/// queue can hand out.
pub(crate) fn control_slots<T: VirtioTransport>(
    transport: &T,
    queue: &VirtQueue<T>,
    request: usize,
    response: usize,
) -> IoResult<ControlSlots<TransportBuffer<T>>> {
    let count = usize::from(queue.size());
    let stride = (request + response).next_multiple_of(SLOT_ALIGNMENT);
    let buffer = if stride == 0 {
        None
    } else {
        let layout = Layout::from_size_align(stride * count, SLOT_ALIGNMENT)
            .map_err(|_| IoError::DeviceFault)?;
        Some(transport.bus().dma().allocate_zeroed(layout)?)
    };
    Ok(ControlSlots {
        buffer,
        count,
        request,
        response,
        stride,
    })
}

impl<B: DmaBuffer> ControlSlots<B> {
    /// Stages `parts` for the chain headed by `head` and translates every
    /// part to a device address.
    ///
    /// # Safety
    ///
    /// The caller must own slot `head`: no chain headed by `head` is in
    /// flight, no reply of an earlier chain headed by `head` is still
    /// being read, and no other task is staging into it. Holding the queue
    /// lock after the completion table reported `head` idle is exactly
    /// that.
    pub(super) unsafe fn stage<P: DmaMemory<Buffer = B>, L: Lend>(
        &self,
        dma: &P,
        head: u16,
        parts: &[Part<'_>],
        lent: &mut L,
        chain: &mut [ChainEntry; MAX_CHAIN_BUFFERS],
    ) -> Result<(usize, ResponseSpan), VirtqueueError> {
        if parts.len() > MAX_CHAIN_BUFFERS {
            return Err(VirtqueueError::ChainTooLong {
                actual: parts.len(),
                limit: MAX_CHAIN_BUFFERS,
            });
        }
        if parts.is_empty() {
            return Err(VirtqueueError::EmptyChain);
        }
        let slot = usize::from(head);
        assert!(
            slot < self.count,
            "virtio descriptor {head} has no control slot among the {} of its queue",
            self.count
        );

        let mut request_used = 0;
        let mut response_used = 0;
        let mut writable_seen = false;
        for (used, part) in parts.iter().enumerate() {
            let (built, writable) = match part {
                Part::Request(bytes) => {
                    let offset = request_used;
                    let end = self.fit(offset, bytes.len(), self.request)?;
                    let address = self.slot_address(slot, offset, bytes.len())?;
                    // SAFETY: `fit` placed `offset..end` inside this
                    // slot's request area, and the caller owns the slot.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            bytes.as_ptr(),
                            self.slot_ptr(slot, offset),
                            bytes.len(),
                        );
                    }
                    request_used = end;
                    (address, false)
                }
                Part::Read(bytes) => (entry(dma, bytes, false)?, false),
                Part::Payload(run) => (foreign_entry(dma, *run)?, false),
                Part::Write(bytes) => (entry(dma, bytes, true)?, true),
                Part::LentRead { buffer, range } => {
                    (entry(dma, lent_bytes(lent, *buffer, range)?, false)?, false)
                }
                Part::LentWrite { buffer, range } => {
                    (entry(dma, lent_bytes(lent, *buffer, range)?, true)?, true)
                }
                Part::Response(len) => {
                    let offset = self.request + response_used;
                    response_used = self.fit(response_used, *len, self.response)?;
                    (self.slot_address(slot, offset, *len)?, true)
                }
            };
            if writable {
                writable_seen = true;
            } else if writable_seen {
                return Err(VirtqueueError::ReadableAfterWritable);
            }
            chain[used] = ChainEntry { writable, ..built };
        }
        Ok((
            parts.len(),
            ResponseSpan {
                offset: self.request,
                len: response_used,
            },
        ))
    }

    /// `used + len`, if it fits an area of `capacity` bytes.
    fn fit(&self, used: usize, len: usize, capacity: usize) -> Result<usize, VirtqueueError> {
        let end = used + len;
        if end > capacity {
            return Err(VirtqueueError::ControlSlotOverflow {
                needed: end,
                capacity,
            });
        }
        Ok(end)
    }

    /// The chain entry for `len` bytes at `offset` inside slot `slot`.
    fn slot_address(
        &self,
        slot: usize,
        offset: usize,
        len: usize,
    ) -> Result<ChainEntry, VirtqueueError> {
        if len == 0 {
            return Err(VirtqueueError::EmptyChain);
        }
        let buffer = self
            .buffer
            .as_ref()
            .expect("a slot area of non-zero length has a buffer");
        Ok(ChainEntry {
            addr: buffer.device_address().get() + (slot * self.stride + offset) as u64,
            len: u32::try_from(len).map_err(|_| VirtqueueError::ChainTooLong {
                actual: len,
                limit: u32::MAX as usize,
            })?,
            writable: false,
        })
    }

    fn slot_ptr(&self, slot: usize, offset: usize) -> *mut u8 {
        let buffer = self
            .buffer
            .as_ref()
            .expect("a slot area of non-zero length has a buffer");
        // SAFETY: `slot < count` and `offset` lies inside the stride, so
        // the result is inside the allocation.
        unsafe { buffer.as_ptr().add(slot * self.stride + offset) }
    }

    /// Whether `address` is a device address inside this table.
    #[cfg(test)]
    pub(crate) fn contains(&self, address: u64) -> bool {
        self.buffer.as_ref().is_some_and(|buffer| {
            let start = buffer.device_address().get();
            (start..start + (self.count * self.stride) as u64).contains(&address)
        })
    }

    /// The first `len` request bytes of slot `head`, read through the
    /// driver's own mapping. A test device whose bus addresses are not
    /// host pointers reads a request this way.
    #[cfg(test)]
    pub(crate) fn request_bytes(&self, head: u16, len: usize) -> alloc::vec::Vec<u8> {
        assert!(
            len <= self.request,
            "a request area holds {} bytes",
            self.request
        );
        // SAFETY: the area lies inside the slot and the driver wrote it
        // before publishing the chain.
        unsafe { core::slice::from_raw_parts(self.slot_ptr(usize::from(head), 0), len) }.to_vec()
    }

    /// Where the response bytes of the chain headed by `head` are.
    pub(crate) fn window(&self, head: u16, span: ResponseSpan) -> ResponseWindow<'_> {
        if span.len == 0 {
            return ResponseWindow::EMPTY;
        }
        ResponseWindow {
            ptr: self.slot_ptr(usize::from(head), span.offset).cast_const(),
            len: span.len,
            _slots: PhantomData,
        }
    }
}

/// The response bytes of one chain, inside its slot.
///
/// Only a [`Reply`](crate::inflight::Reply) reads through it.
#[derive(Clone, Copy)]
pub(crate) struct ResponseWindow<'a> {
    ptr: *const u8,
    len: usize,
    _slots: PhantomData<&'a [u8]>,
}

// SAFETY: the window points into a `DmaBuffer`, which is `Send + Sync`,
// borrowed for `'a`; reading it is guarded by the slot ownership protocol
// in the module docs.
unsafe impl Send for ResponseWindow<'_> {}
// SAFETY: as above.
unsafe impl Sync for ResponseWindow<'_> {}

impl ResponseWindow<'_> {
    pub(crate) const EMPTY: Self = Self {
        ptr: core::ptr::null(),
        len: 0,
        _slots: PhantomData,
    };

    /// The response bytes.
    ///
    /// # Safety
    ///
    /// The chain this window belongs to has completed and its slot is
    /// still held by the caller, so the device no longer writes it and no
    /// other request stages into it.
    pub(crate) unsafe fn bytes(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: the pointer and length name bytes of the slot, which
        // are initialised (the table is zeroed) and, by the caller's
        // contract, not written meanwhile.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// Fixed-size buffers the device fills on its own, carved from its DMA
/// memory once.
///
/// The receive and event rings post a buffer, wait for the device to fill
/// it and post it again; the driver, not the descriptor identifier, picks
/// the slot and holds the whole pool through `&mut`, so access needs no
/// protocol beyond the borrow.
pub(crate) struct SlotPool<B: DmaBuffer> {
    buffer: B,
    slot_bytes: usize,
    count: usize,
}

impl<B: DmaBuffer> SlotPool<B> {
    pub(crate) fn new<P: DmaMemory<Buffer = B>>(
        dma: &P,
        count: usize,
        slot_bytes: usize,
    ) -> IoResult<Self> {
        let layout = Layout::from_size_align(count * slot_bytes, SLOT_ALIGNMENT)
            .map_err(|_| IoError::DeviceFault)?;
        Ok(Self {
            buffer: dma.allocate_zeroed(layout)?,
            slot_bytes,
            count,
        })
    }

    /// Slot `index`.
    ///
    /// # Panics
    ///
    /// When `index` is not a slot of this pool: a caller bug, since every
    /// index comes from the pool's own bookkeeping.
    pub(crate) fn slot_mut(&mut self, index: usize) -> &mut [u8] {
        assert!(
            index < self.count,
            "slot {index} is outside the {} of this pool",
            self.count
        );
        // SAFETY: the slot lies inside the allocation, and `&mut self`
        // is the only handle to it.
        unsafe {
            core::slice::from_raw_parts_mut(
                self.buffer.as_ptr().add(index * self.slot_bytes),
                self.slot_bytes,
            )
        }
    }
}

/// The chain `parts` of one bring-up request, polled to completion with
/// its response copied into `out`.
///
/// For the one moment a driver may spin on a ring, before the executor
/// exists: the caller holds the queue exclusively, so nothing else submits
/// while the request runs.
pub(crate) fn exchange_blocking<T: VirtioTransport, const N: usize>(
    inflight: &crate::inflight::InFlight<N>,
    queue: &mut VirtQueue<T>,
    transport: &T,
    slots: &ControlSlots<TransportBuffer<T>>,
    parts: &[Part<'_>],
    out: &mut [u8],
) -> IoResult<u32> {
    let head = queue.next_free_descriptor();
    if !inflight.is_idle(head) {
        // A request abandoned on this identifier still owns its slot.
        return Err(IoError::DeviceFault);
    }
    // SAFETY: `queue` is held exclusively and the completion table says
    // `head` is idle, so the slot is ours.
    let (token, span) = unsafe { queue.submit_parts(transport, slots, parts, &mut ())? };
    queue.notify(transport);
    let written = queue.reap_blocking(transport, token)?;
    let window = slots.window(token, span);
    // SAFETY: the chain completed and nothing else uses the slot.
    let response = unsafe { window.bytes() };
    let copied = out.len().min(response.len());
    out[..copied].copy_from_slice(&response[..copied]);
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::{ControlSlots, Part, control_slots};
    use crate::bus::DeviceBus;
    use crate::features::NegotiatedFeatures;
    use crate::queue::{ChainEntry, MAX_CHAIN_BUFFERS, TransportBuffer, VirtQueue, VirtqueueError};
    use crate::testing::{ConfinedTransport, FakeTransport, FakeTransportConfig};
    use crate::transport::VirtioFeatures;
    use crate::transport::VirtioTransport;
    use helios_hal::dma::DmaBuffer;

    const REQUEST: usize = 8;
    const RESPONSE: usize = 8;

    fn table() -> (
        ConfinedTransport,
        ControlSlots<TransportBuffer<ConfinedTransport>>,
    ) {
        let transport = FakeTransport::confined(FakeTransportConfig::default());
        let queue = VirtQueue::new(
            &transport,
            0,
            8,
            4,
            NegotiatedFeatures::from_bits(VirtioFeatures::VERSION_1.bits()),
        )
        .expect("queue should initialize");
        let slots = control_slots(&transport, &queue, REQUEST, RESPONSE)
            .expect("the slots should allocate");
        (transport, slots)
    }

    fn stage(
        transport: &ConfinedTransport,
        slots: &ControlSlots<TransportBuffer<ConfinedTransport>>,
        head: u16,
        parts: &[Part<'_>],
    ) -> Result<usize, VirtqueueError> {
        let mut chain = [ChainEntry {
            addr: 0,
            len: 0,
            writable: false,
        }; MAX_CHAIN_BUFFERS];
        // SAFETY: the test owns every slot of its own table.
        unsafe { slots.stage(transport.bus().dma(), head, parts, &mut (), &mut chain) }
            .map(|(len, _)| len)
    }

    #[test]
    fn request_parts_are_packed_into_the_slot_in_order() {
        let (transport, slots) = table();
        let used = stage(
            &transport,
            &slots,
            3,
            &[
                Part::Request(&[1, 2, 3]),
                Part::Request(&[4, 5]),
                Part::Response(4),
            ],
        )
        .expect("the parts fit the slot");
        assert_eq!(used, 3);
        assert_eq!(slots.request_bytes(3, 5), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_request_larger_than_its_area_is_refused_not_truncated() {
        let (transport, slots) = table();
        assert_eq!(
            stage(&transport, &slots, 0, &[Part::Request(&[0; REQUEST + 1])]).err(),
            Some(VirtqueueError::ControlSlotOverflow {
                needed: REQUEST + 1,
                capacity: REQUEST,
            })
        );
        assert_eq!(
            stage(&transport, &slots, 0, &[Part::Response(RESPONSE + 1)]).err(),
            Some(VirtqueueError::ControlSlotOverflow {
                needed: RESPONSE + 1,
                capacity: RESPONSE,
            })
        );
    }

    #[test]
    fn a_readable_part_after_a_writable_one_is_refused() {
        let (transport, slots) = table();
        assert_eq!(
            stage(
                &transport,
                &slots,
                0,
                &[Part::Response(4), Part::Request(&[1])]
            )
            .err(),
            Some(VirtqueueError::ReadableAfterWritable)
        );
    }

    #[test]
    fn every_descriptor_has_a_slot_of_its_own() {
        let (transport, slots) = table();
        let mut starts = alloc::vec::Vec::new();
        for head in 0..8 {
            let mut chain = [ChainEntry {
                addr: 0,
                len: 0,
                writable: false,
            }; MAX_CHAIN_BUFFERS];
            // SAFETY: the test owns every slot of its own table.
            unsafe {
                slots.stage(
                    transport.bus().dma(),
                    head,
                    &[Part::Request(&[head as u8])],
                    &mut (),
                    &mut chain,
                )
            }
            .expect("the request fits");
            assert!(slots.contains(chain[0].addr));
            starts.push(chain[0].addr);
        }
        starts.sort_unstable();
        starts.dedup();
        assert_eq!(starts.len(), 8, "no two descriptors share a slot");
    }

    #[test]
    fn lent_parts_name_the_lent_buffer_in_place() {
        let (transport, slots) = table();
        let buffer = transport.pool().buffer(64);
        let start = buffer.device_address().get();
        let mut lent = [buffer];
        let mut chain = [ChainEntry {
            addr: 0,
            len: 0,
            writable: false,
        }; MAX_CHAIN_BUFFERS];
        // SAFETY: the test owns every slot of its own table.
        let (used, _) = unsafe {
            slots.stage(
                transport.bus().dma(),
                0,
                &[
                    Part::LentRead {
                        buffer: 0,
                        range: 0..16,
                    },
                    Part::LentWrite {
                        buffer: 0,
                        range: 16..64,
                    },
                ],
                &mut lent,
                &mut chain,
            )
        }
        .expect("the lent ranges are inside the buffer");
        assert_eq!(used, 2);
        assert_eq!(
            (chain[0].addr, chain[0].len, chain[0].writable),
            (start, 16, false)
        );
        assert_eq!(
            (chain[1].addr, chain[1].len, chain[1].writable),
            (start + 16, 48, true)
        );
    }

    #[test]
    fn a_lent_range_past_its_buffer_is_refused() {
        let (transport, slots) = table();
        let mut lent = [transport.pool().buffer(64)];
        let mut chain = [ChainEntry {
            addr: 0,
            len: 0,
            writable: false,
        }; MAX_CHAIN_BUFFERS];
        for (buffer, end, len) in [(0, 65, 64), (1, 8, 0)] {
            // SAFETY: the test owns every slot of its own table.
            let refused = unsafe {
                slots.stage(
                    transport.bus().dma(),
                    0,
                    &[Part::LentWrite {
                        buffer,
                        range: 0..end,
                    }],
                    &mut lent,
                    &mut chain,
                )
            };
            assert_eq!(
                refused.err(),
                Some(VirtqueueError::LentOutOfRange { buffer, end, len })
            );
        }
    }
}
