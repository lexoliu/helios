//! The split virtqueue layout of virtio 1.0 (§2.7).
//!
//! Three device-visible areas: a descriptor table, a driver-owned
//! available ring, and a device-owned used ring. Descriptor identifiers
//! are indices into the table, so a chain of `n` buffers costs `n`
//! identifiers unless it is pushed into an indirect table, which costs
//! one.

use core::alloc::Layout;
use core::mem::size_of;
use core::sync::atomic::{Ordering, fence};

use alloc::boxed::Box;

use crate::bus::{DeviceBus, DmaBuffer, DmaPool};
use crate::features::NegotiatedFeatures;
use crate::transport::VirtioTransport;

use super::{
    ChainEntry, DESCRIPTOR_BYTES, MAX_CHAIN_BUFFERS, RingCore, RingOps, TransportBuffer,
    VirtqueueError, need_event,
};

const DESC_FLAG_NEXT: u16 = 1;
const DESC_FLAG_WRITE: u16 = 2;
const DESC_FLAG_INDIRECT: u16 = 4;
const USED_FLAG_NO_NOTIFY: u16 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Descriptor {
    pub(super) addr: u64,
    pub(super) len: u32,
    pub(super) flags: u16,
    pub(super) next: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct UsedElem {
    pub(super) id: u32,
    pub(super) len: u32,
}

pub(super) struct SplitRing<T: VirtioTransport> {
    core: RingCore<TransportBuffer<T>>,
    descriptors: TransportBuffer<T>,
    driver_area: TransportBuffer<T>,
    device_area: TransportBuffer<T>,
    desc_shadow: Box<[Descriptor]>,
    /// Next available-ring slot the driver will fill. Free running, so
    /// it doubles as the value published in the available index.
    avail_idx: u16,
    last_used_idx: u16,
}

impl<T: VirtioTransport> RingOps<T> for SplitRing<T> {
    fn new(
        transport: &T,
        index: u16,
        size: u16,
        chain_limit: u16,
        features: NegotiatedFeatures,
    ) -> Result<Self, VirtqueueError> {
        let dma = transport.bus().dma();
        let descriptor_layout =
            Layout::from_size_align(DESCRIPTOR_BYTES * usize::from(size), DESCRIPTOR_BYTES)
                .map_err(|_| VirtqueueError::RingAllocation)?;
        let driver_layout = Layout::from_size_align(driver_area_len(size), 2)
            .map_err(|_| VirtqueueError::RingAllocation)?;
        let device_layout = Layout::from_size_align(device_area_len(size), 4)
            .map_err(|_| VirtqueueError::RingAllocation)?;

        let ring = Self {
            core: RingCore::new(dma, size, chain_limit, features)?,
            descriptors: dma
                .allocate_zeroed(descriptor_layout)
                .map_err(|_| VirtqueueError::RingAllocation)?,
            driver_area: dma
                .allocate_zeroed(driver_layout)
                .map_err(|_| VirtqueueError::RingAllocation)?,
            device_area: dma
                .allocate_zeroed(device_layout)
                .map_err(|_| VirtqueueError::RingAllocation)?,
            desc_shadow: alloc::vec![Descriptor::default(); usize::from(size)].into_boxed_slice(),
            avail_idx: 0,
            last_used_idx: 0,
        };
        ring.program(transport, index);
        Ok(ring)
    }

    fn submit(&mut self, chain: &[ChainEntry], publish: bool) -> Result<u16, VirtqueueError> {
        self.core.check_chain(chain)?;
        let indirect = self.core.use_indirect(chain.len());
        let needed = self.core.descriptors_needed(chain.len());
        if usize::from(self.core.ids.available()) < needed {
            return Err(VirtqueueError::Full { needed });
        }

        let head = if indirect {
            self.write_indirect_chain(chain)
        } else {
            self.write_linked_chain(chain)
        };

        self.core.record_chain(head, chain);
        let slot = self.avail_idx & (self.core.size - 1);
        self.write_avail_ring(slot, head);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.core.num_added = self.core.num_added.wrapping_add(1);
        if publish {
            self.publish();
        }
        Ok(head)
    }

    fn publish(&mut self) {
        // The available ring entries and the buffers they name must be
        // visible before the index that exposes them.
        fence(Ordering::Release);
        self.write_avail_idx(self.avail_idx);
    }

    fn has_pending_used(&self) -> bool {
        self.read_used_idx() != self.last_used_idx
    }

    fn pop_used(&mut self) -> Option<(u16, u32)> {
        if self.core.features.in_order() {
            self.pop_used_in_order()
        } else {
            self.pop_used_unordered()
        }
    }

    fn should_notify(&self) -> bool {
        // Store-load barrier (virtio 1.2 §2.7.10, Linux's
        // virtqueue_kick_prepare): the available index store must be
        // visible to the device before its suppression state is read, or
        // a device that just re-enabled notifications can miss the entry
        // while the driver reads a stale avail_event and suppresses the
        // kick, stalling the queue until the next submission.
        fence(Ordering::SeqCst);
        if self.core.features.event_idx() {
            let new = self.avail_idx;
            let old = new.wrapping_sub(self.core.num_added);
            return need_event(self.read_avail_event(), new, old);
        }
        self.read_used_flags() & USED_FLAG_NO_NOTIFY == 0
    }

    fn notification_data(&self, index: u16) -> u32 {
        u32::from(index) | (u32::from(self.avail_idx) << 16)
    }

    fn clear_added(&mut self) {
        self.core.num_added = 0;
    }

    fn available(&self) -> usize {
        usize::from(self.core.ids.available())
    }

    fn has_room_for(&self, buffers: usize) -> bool {
        usize::from(self.core.ids.available()) >= self.core.descriptors_needed(buffers)
    }

    fn next_id(&self) -> u16 {
        self.core.ids.peek()
    }

    fn reprogram(&mut self, transport: &T, index: u16) {
        zero(&self.descriptors);
        zero(&self.driver_area);
        zero(&self.device_area);
        self.desc_shadow.fill(Descriptor::default());
        self.avail_idx = 0;
        self.last_used_idx = 0;
        self.core.clear();
        self.program(transport, index);
    }
}

impl<T: VirtioTransport> SplitRing<T> {
    fn program(&self, transport: &T, index: u16) {
        transport.set_queue(
            index,
            self.core.size,
            self.descriptors.phys_addr(),
            self.driver_area.phys_addr(),
            self.device_area.phys_addr(),
        );
    }

    /// Writes a chain into the head's pre-allocated indirect table and
    /// returns the single ring descriptor that points at it.
    fn write_indirect_chain(&mut self, chain: &[ChainEntry]) -> u16 {
        let head = self
            .core
            .ids
            .allocate()
            .expect("indirect chain capacity was checked");
        let tables = self
            .core
            .indirect
            .as_ref()
            .expect("indirect submission without indirect tables");
        assert!(
            chain.len() <= tables.entries(),
            "indirect chain exceeds the table capacity"
        );
        let table = tables.table_ptr(head).cast::<Descriptor>();
        for (position, entry) in chain.iter().enumerate() {
            let last = position + 1 == chain.len();
            let descriptor = Descriptor {
                addr: entry.addr,
                len: entry.len,
                flags: entry_flags(entry.writable, !last),
                next: if last { 0 } else { (position + 1) as u16 },
            };
            unsafe {
                table.add(position).write_volatile(descriptor);
            }
        }

        let descriptor = Descriptor {
            addr: tables.table_addr(head),
            len: (chain.len() * DESCRIPTOR_BYTES) as u32,
            flags: DESC_FLAG_INDIRECT,
            next: 0,
        };
        self.desc_shadow[usize::from(head)] = descriptor;
        self.write_desc(head);
        head
    }

    /// Writes a chain as linked entries of the main descriptor table.
    fn write_linked_chain(&mut self, chain: &[ChainEntry]) -> u16 {
        let mut ids = [0_u16; MAX_CHAIN_BUFFERS];
        for slot in ids.iter_mut().take(chain.len()) {
            *slot = self
                .core
                .ids
                .allocate()
                .expect("chain capacity was checked before allocating");
        }

        for (position, entry) in chain.iter().enumerate() {
            let last = position + 1 == chain.len();
            let id = ids[position];
            self.desc_shadow[usize::from(id)] = Descriptor {
                addr: entry.addr,
                len: entry.len,
                flags: entry_flags(entry.writable, !last),
                next: if last { 0 } else { ids[position + 1] },
            };
            self.write_desc(id);
        }
        ids[0]
    }

    fn pop_used_unordered(&mut self) -> Option<(u16, u32)> {
        if self.read_used_idx() == self.last_used_idx {
            return None;
        }
        fence(Ordering::Acquire);
        let elem = self.read_used_elem(self.last_used_idx & (self.core.size - 1));
        let head = self.finish_completion(elem.id as u16);
        Some((head, elem.len))
    }

    /// VIRTIO_F_IN_ORDER completion (virtio 1.2 §2.7.9).
    ///
    /// The device may report a whole batch of chains with one used entry
    /// naming the last chain of the batch, skipping the ring entries in
    /// front of it. The entry sitting at the driver's own position is
    /// therefore only trustworthy when it names the chain the driver
    /// submitted first; otherwise the authoritative entry is the one at
    /// the end of what the device published, and the chains in between
    /// are, per the specification, buffers the device used completely.
    fn pop_used_in_order(&mut self) -> Option<(u16, u32)> {
        if self.core.batch.remaining == 0 {
            let used_idx = self.read_used_idx();
            if used_idx == self.last_used_idx {
                return None;
            }
            fence(Ordering::Acquire);
            let mask = self.core.size - 1;
            let front = self
                .core
                .order
                .front()
                .expect("virtqueue reported a completion with nothing in flight");
            let here = self.read_used_elem(self.last_used_idx & mask);
            let (final_id, final_len) = if here.id as u16 == front {
                (front, here.len)
            } else {
                let tail = self.read_used_elem(used_idx.wrapping_sub(1) & mask);
                (tail.id as u16, tail.len)
            };
            self.core.begin_in_order_batch(final_id, final_len);
        }

        let (id, len) = self.core.next_in_order_completion();
        self.finish_completion(id);
        Some((id, len))
    }

    /// Advances the used cursor, republishes the used event and returns
    /// the chain's descriptors to the pool.
    fn finish_completion(&mut self, head: u16) -> u16 {
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        if self.core.features.event_idx() {
            self.write_used_event(self.last_used_idx);
            // Store-load barrier (Linux's virtqueue_enable_cb_prepare +
            // virtqueue_poll): used_event must reach the device before
            // the caller's next used index read decides the ring is
            // empty, or the device can publish an entry against the
            // stale used_event, skip the interrupt, and leave the caller
            // asleep.
            fence(Ordering::SeqCst);
        }
        self.release_chain(head);
        head
    }

    fn release_chain(&mut self, head: u16) {
        let mut cursor = head;
        loop {
            let descriptor = self.desc_shadow[usize::from(cursor)];
            self.core.ids.release(cursor);
            if descriptor.flags & DESC_FLAG_NEXT == 0 {
                break;
            }
            cursor = descriptor.next;
        }
    }

    fn write_desc(&self, index: u16) {
        unsafe {
            self.descriptors
                .as_ptr()
                .cast::<Descriptor>()
                .add(usize::from(index))
                .write_volatile(self.desc_shadow[usize::from(index)]);
        }
    }

    fn write_avail_ring(&self, slot: u16, head: u16) {
        unsafe {
            self.driver_area
                .as_ptr()
                .cast::<u16>()
                .add(2 + usize::from(slot))
                .write_volatile(head);
        }
    }

    fn write_avail_idx(&self, index: u16) {
        unsafe {
            self.driver_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .write_volatile(index);
        }
    }

    fn read_used_idx(&self) -> u16 {
        unsafe {
            self.device_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .read_volatile()
        }
    }

    fn read_used_flags(&self) -> u16 {
        unsafe { self.device_area.as_ptr().cast::<u16>().read_volatile() }
    }

    /// Device-written avail_event: the available index after which the
    /// device wants its next notification.
    fn read_avail_event(&self) -> u16 {
        unsafe {
            self.device_area
                .as_ptr()
                .add(4 + usize::from(self.core.size) * size_of::<UsedElem>())
                .cast::<u16>()
                .read_volatile()
        }
    }

    /// Publishes how far the driver has consumed the used ring so the
    /// device can suppress interrupts for completions already seen.
    fn write_used_event(&self, index: u16) {
        unsafe {
            self.driver_area
                .as_ptr()
                .add(4 + usize::from(self.core.size) * 2)
                .cast::<u16>()
                .write_volatile(index);
        }
    }

    fn read_used_elem(&self, slot: u16) -> UsedElem {
        unsafe {
            self.device_area
                .as_ptr()
                .add(4 + (usize::from(slot) * size_of::<UsedElem>()))
                .cast::<UsedElem>()
                .read_volatile()
        }
    }
}

fn entry_flags(writable: bool, has_next: bool) -> u16 {
    let mut flags = 0;
    if writable {
        flags |= DESC_FLAG_WRITE;
    }
    if has_next {
        flags |= DESC_FLAG_NEXT;
    }
    flags
}

fn zero<B: DmaBuffer>(buffer: &B) {
    unsafe {
        buffer.as_ptr().write_bytes(0, buffer.len());
    }
}

fn driver_area_len(size: u16) -> usize {
    4 + (usize::from(size) * 2) + 2
}

fn device_area_len(size: u16) -> usize {
    4 + (usize::from(size) * size_of::<UsedElem>()) + 2
}

#[cfg(test)]
impl<T: VirtioTransport> SplitRing<T> {
    /// Descriptor `index` as the device would read it.
    pub(super) fn descriptor(&self, index: u16) -> Descriptor {
        unsafe {
            self.descriptors
                .as_ptr()
                .cast::<Descriptor>()
                .add(usize::from(index))
                .read_volatile()
        }
    }

    /// Entry `slot` of the indirect table belonging to chain `id`.
    pub(super) fn indirect_descriptor(&self, id: u16, slot: usize) -> Descriptor {
        let tables = self
            .core
            .indirect
            .as_ref()
            .expect("queue has no indirect tables");
        unsafe {
            tables
                .table_ptr(id)
                .cast::<Descriptor>()
                .add(slot)
                .read_volatile()
        }
    }

    /// The available index the device can currently see.
    pub(super) fn published_avail_idx(&self) -> u16 {
        unsafe {
            self.driver_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .read_volatile()
        }
    }

    /// The used event the driver last published.
    pub(super) fn published_used_event(&self) -> u16 {
        unsafe {
            self.driver_area
                .as_ptr()
                .add(4 + usize::from(self.core.size) * 2)
                .cast::<u16>()
                .read_volatile()
        }
    }

    /// Device side: suppress or allow notifications without event index.
    pub(super) fn device_set_used_flags(&self, flags: u16) {
        unsafe {
            self.device_area
                .as_ptr()
                .cast::<u16>()
                .write_volatile(flags);
        }
    }

    /// Device side: publish the available index it wants a kick at.
    pub(super) fn device_set_avail_event(&self, value: u16) {
        unsafe {
            self.device_area
                .as_ptr()
                .add(4 + usize::from(self.core.size) * size_of::<UsedElem>())
                .cast::<u16>()
                .write_volatile(value);
        }
    }

    /// Device side: the buffers of chain `id`, in chain order, as
    /// address/length/writable triples.
    ///
    /// Driver tests need to read the request a driver wrote and answer it
    /// the way a device would; both start from the chain the driver
    /// published, whether it went into the ring directly or into this
    /// chain's indirect table.
    #[cfg(test)]
    pub(super) fn device_chain(&self, id: u16) -> alloc::vec::Vec<(u64, u32, bool)> {
        let head = self.descriptor(id);
        let mut buffers = alloc::vec::Vec::new();
        if head.flags & DESC_FLAG_INDIRECT != 0 {
            let entries = usize::try_from(head.len).expect("indirect table length fits a usize")
                / DESCRIPTOR_BYTES;
            for slot in 0..entries {
                let descriptor = self.indirect_descriptor(id, slot);
                buffers.push((
                    descriptor.addr,
                    descriptor.len,
                    descriptor.flags & DESC_FLAG_WRITE != 0,
                ));
            }
            return buffers;
        }
        let mut descriptor = head;
        loop {
            buffers.push((
                descriptor.addr,
                descriptor.len,
                descriptor.flags & DESC_FLAG_WRITE != 0,
            ));
            if descriptor.flags & DESC_FLAG_NEXT == 0 {
                return buffers;
            }
            descriptor = self.descriptor(descriptor.next);
        }
    }

    /// Device side: complete one chain by its identifier.
    pub(super) fn device_complete(&self, id: u16, len: u32) {
        let used_idx = self.read_used_idx();
        let slot = used_idx & (self.core.size - 1);
        unsafe {
            self.device_area
                .as_ptr()
                .add(4 + usize::from(slot) * size_of::<UsedElem>())
                .cast::<UsedElem>()
                .write_volatile(UsedElem {
                    id: u32::from(id),
                    len,
                });
            self.device_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .write_volatile(used_idx.wrapping_add(1));
        }
    }

    /// Device side: report `chains` completions with a single used entry
    /// naming the batch's last chain, as VIRTIO_F_IN_ORDER allows.
    pub(super) fn device_complete_batch(&self, final_id: u16, len: u32, chains: u16) {
        let used_idx = self.read_used_idx();
        let slot = used_idx.wrapping_add(chains - 1) & (self.core.size - 1);
        unsafe {
            self.device_area
                .as_ptr()
                .add(4 + usize::from(slot) * size_of::<UsedElem>())
                .cast::<UsedElem>()
                .write_volatile(UsedElem {
                    id: u32::from(final_id),
                    len,
                });
            self.device_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .write_volatile(used_idx.wrapping_add(chains));
        }
    }
}
