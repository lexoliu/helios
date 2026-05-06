use alloc::boxed::Box;
use core::alloc::Layout;
use core::mem::size_of;
use core::sync::atomic::{Ordering, fence};

use helios_hal::io::{IoError, IoResult};

use crate::bus::{DeviceBus, DmaBuffer, DmaPool};
use crate::transport::VirtioTransport;

const DESC_FLAG_NEXT: u16 = 1;
const DESC_FLAG_WRITE: u16 = 2;
const USED_FLAG_NO_NOTIFY: u16 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Descriptor {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UsedElem {
    id: u32,
    len: u32,
}

pub struct VirtQueue<T: VirtioTransport> {
    index: u16,
    size: u16,
    descriptors: <<T::Bus as DeviceBus>::DmaPool as DmaPool>::Buffer,
    driver_area: <<T::Bus as DeviceBus>::DmaPool as DmaPool>::Buffer,
    device_area: <<T::Bus as DeviceBus>::DmaPool as DmaPool>::Buffer,
    desc_shadow: Box<[Descriptor]>,
    free_head: u16,
    num_used: u16,
    avail_idx: u16,
    last_used_idx: u16,
}

impl<T: VirtioTransport> VirtQueue<T> {
    pub fn new(transport: &T, index: u16, size: u16) -> IoResult<Self> {
        if size == 0 || !size.is_power_of_two() {
            return Err(IoError::Unsupported);
        }

        let descriptor_layout =
            Layout::from_size_align(size_of::<Descriptor>() * usize::from(size), 16)
                .map_err(|_| IoError::DeviceFault)?;
        let driver_layout =
            Layout::from_size_align(driver_area_len(size), 2).map_err(|_| IoError::DeviceFault)?;
        let device_layout =
            Layout::from_size_align(device_area_len(size), 4).map_err(|_| IoError::DeviceFault)?;
        let dma = transport.bus().dma();

        let descriptors = dma.allocate_zeroed(descriptor_layout)?;
        let driver_area = dma.allocate_zeroed(driver_layout)?;
        let device_area = dma.allocate_zeroed(device_layout)?;
        let mut desc_shadow: Box<[Descriptor]> = (0..size).map(|_| Descriptor::default()).collect();

        for index in 0..(size - 1) {
            desc_shadow[usize::from(index)].next = index + 1;
        }

        transport.set_queue(
            index,
            size,
            descriptors.phys_addr(),
            driver_area.phys_addr(),
            device_area.phys_addr(),
        );

        Ok(Self {
            index,
            size,
            descriptors,
            driver_area,
            device_area,
            desc_shadow,
            free_head: 0,
            num_used: 0,
            avail_idx: 0,
            last_used_idx: 0,
        })
    }

    pub fn submit(
        &mut self,
        transport: &T,
        inputs: &[&[u8]],
        outputs: &mut [&mut [u8]],
    ) -> IoResult<u16> {
        if inputs.is_empty() && outputs.is_empty() {
            return Err(IoError::DeviceFault);
        }

        let descriptors_needed = inputs.len() + outputs.len();
        if self.num_used as usize + descriptors_needed > usize::from(self.size) {
            return Err(IoError::DeviceFault);
        }

        let head = self.free_head;
        let mut last = self.free_head;

        for buffer in inputs {
            last = self.push_descriptor(transport, buffer, false)?;
        }

        for output in outputs.iter_mut() {
            last = self.push_descriptor(transport, output, true)?;
        }

        self.desc_shadow[usize::from(last)].flags &= !DESC_FLAG_NEXT;
        self.write_desc(last);

        let slot = self.avail_idx & (self.size - 1);
        self.write_avail_ring(slot, head);
        fence(Ordering::Release);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.write_avail_idx(self.avail_idx);
        self.num_used += descriptors_needed as u16;

        Ok(head)
    }

    pub fn pop_used(&mut self) -> Option<u16> {
        self.pop_used_with_len().map(|(head, _)| head)
    }

    pub fn submit_read_only_deferred(&mut self, transport: &T, input: &[u8]) -> IoResult<u16> {
        if input.is_empty() {
            return Err(IoError::DeviceFault);
        }
        if self.num_used == self.size {
            return Err(IoError::DeviceFault);
        }

        let head = self.free_head;
        self.free_head = self.desc_shadow[usize::from(head)].next;
        self.write_descriptor_with_flags(transport, head, input, 0)?;
        self.num_used += 1;
        Ok(head)
    }

    pub(crate) fn submit_output_at(
        &mut self,
        transport: &T,
        token: u16,
        output: &mut [u8],
    ) -> IoResult<u16> {
        if output.is_empty() {
            return Err(IoError::DeviceFault);
        }
        if self.num_used == self.size {
            return Err(IoError::DeviceFault);
        }

        self.remove_free_descriptor(token);
        self.write_descriptor(transport, token, output, true)?;
        self.desc_shadow[usize::from(token)].flags &= !DESC_FLAG_NEXT;
        self.write_desc(token);
        self.stage_deferred_head(token);
        self.publish_deferred_heads();
        self.num_used += 1;
        Ok(token)
    }

    pub(crate) fn stage_deferred_head(&mut self, head: u16) {
        let slot = self.avail_idx & (self.size - 1);
        self.write_avail_ring(slot, head);
        self.avail_idx = self.avail_idx.wrapping_add(1);
    }

    pub(crate) fn publish_deferred_heads(&mut self) {
        fence(Ordering::Release);
        self.write_avail_idx(self.avail_idx);
    }

    pub fn available_descriptors(&self) -> usize {
        usize::from(self.size - self.num_used)
    }

    pub fn next_free_descriptor(&self) -> u16 {
        self.free_head
    }

    pub fn pop_used_with_len(&mut self) -> Option<(u16, u32)> {
        let used_idx = self.read_used_idx();
        if used_idx == self.last_used_idx {
            return None;
        }

        fence(Ordering::Acquire);
        let slot = self.last_used_idx & (self.size - 1);
        let elem = self.read_used_elem(slot);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        let head = elem.id as u16;
        self.recycle_chain(head);
        Some((head, elem.len))
    }

    pub fn notify(&self, transport: &T) {
        if self.should_notify() {
            transport.notify_queue(self.index);
        }
    }

    fn push_descriptor(&mut self, transport: &T, buffer: &[u8], writable: bool) -> IoResult<u16> {
        assert!(!buffer.is_empty(), "virtqueue buffers must not be empty");

        let desc_index = self.free_head;
        self.free_head = self.desc_shadow[usize::from(desc_index)].next;
        self.write_descriptor(transport, desc_index, buffer, writable)?;
        Ok(desc_index)
    }

    fn write_descriptor(
        &mut self,
        transport: &T,
        desc_index: u16,
        buffer: &[u8],
        writable: bool,
    ) -> IoResult<()> {
        let mut flags = DESC_FLAG_NEXT;
        if writable {
            flags |= DESC_FLAG_WRITE;
        }
        self.write_descriptor_with_flags(transport, desc_index, buffer, flags)
    }

    fn write_descriptor_with_flags(
        &mut self,
        transport: &T,
        desc_index: u16,
        buffer: &[u8],
        flags: u16,
    ) -> IoResult<()> {
        let desc = &mut self.desc_shadow[usize::from(desc_index)];
        let addr = transport.bus().dma().dma_addr(buffer.as_ptr())?;

        *desc = Descriptor {
            addr,
            len: buffer.len() as u32,
            flags,
            next: desc.next,
        };
        self.write_desc(desc_index);
        Ok(())
    }

    fn remove_free_descriptor(&mut self, token: u16) {
        assert!(
            token < self.size,
            "virtqueue free descriptor token is out of range"
        );
        if self.free_head == token {
            self.free_head = self.desc_shadow[usize::from(token)].next;
            return;
        }

        let mut previous = self.free_head;
        for _ in 0..self.size {
            let next = self.desc_shadow[usize::from(previous)].next;
            assert!(
                next != previous,
                "virtqueue free descriptor list contains a cycle"
            );
            if next == token {
                self.desc_shadow[usize::from(previous)].next =
                    self.desc_shadow[usize::from(token)].next;
                return;
            }
            assert!(
                next < self.size,
                "virtqueue free descriptor {token} is not present"
            );
            previous = next;
        }
        panic!("virtqueue free descriptor {token} is not present");
    }

    fn recycle_chain(&mut self, head: u16) {
        let mut cursor = head;
        let mut returned = 0u16;

        loop {
            returned = returned.wrapping_add(1);
            let next = self.desc_shadow[usize::from(cursor)].next;
            let has_next = self.desc_shadow[usize::from(cursor)].flags & DESC_FLAG_NEXT != 0;
            self.desc_shadow[usize::from(cursor)].next = self.free_head;
            self.free_head = cursor;
            if !has_next {
                break;
            }
            cursor = next;
        }

        let previous = self.num_used;
        self.num_used = self.num_used.wrapping_sub(returned);
        assert!(previous >= returned, "virtqueue used descriptor underflow");
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

    fn should_notify(&self) -> bool {
        self.read_used_flags() & USED_FLAG_NO_NOTIFY == 0
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

fn driver_area_len(size: u16) -> usize {
    4 + (usize::from(size) * 2) + 2
}

fn device_area_len(size: u16) -> usize {
    4 + (usize::from(size) * size_of::<UsedElem>()) + 2
}

#[cfg(test)]
mod tests {
    use alloc::alloc::{alloc_zeroed, dealloc};
    use core::alloc::Layout;
    use core::mem::size_of;
    use core::ptr::NonNull;

    use helios_hal::io::{IoError, IoResult};

    use super::{Descriptor, USED_FLAG_NO_NOTIFY, UsedElem, VirtQueue};
    use crate::bus::{DeviceBus, DmaBuffer, DmaPool};
    use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

    #[derive(Clone, Copy, Default)]
    struct FakeDmaPool;

    struct FakeDmaBuffer {
        ptr: NonNull<u8>,
        layout: Layout,
    }

    #[derive(Clone, Copy, Default)]
    struct FakeBus {
        dma: FakeDmaPool,
    }

    struct FakeTransport {
        bus: FakeBus,
    }

    impl DmaPool for FakeDmaPool {
        type Buffer = FakeDmaBuffer;

        fn allocate_zeroed(&self, layout: Layout) -> IoResult<Self::Buffer> {
            let ptr = unsafe { alloc_zeroed(layout) };
            let ptr = NonNull::new(ptr).ok_or(IoError::DeviceFault)?;
            Ok(FakeDmaBuffer { ptr, layout })
        }

        fn dma_addr(&self, ptr: *const u8) -> IoResult<u64> {
            Ok(ptr as usize as u64)
        }
    }

    impl DmaBuffer for FakeDmaBuffer {
        fn phys_addr(&self) -> u64 {
            self.ptr.as_ptr() as usize as u64
        }

        fn as_ptr(&self) -> *mut u8 {
            self.ptr.as_ptr()
        }

        fn len(&self) -> usize {
            self.layout.size()
        }
    }

    impl Drop for FakeDmaBuffer {
        fn drop(&mut self) {
            unsafe {
                dealloc(self.ptr.as_ptr(), self.layout);
            }
        }
    }

    impl DeviceBus for FakeBus {
        type DmaPool = FakeDmaPool;

        fn read_u32(&self, _offset: usize) -> u32 {
            0
        }

        fn write_u32(&self, _offset: usize, _value: u32) {}

        fn dma(&self) -> &Self::DmaPool {
            &self.dma
        }
    }

    impl VirtioTransport for FakeTransport {
        type Bus = FakeBus;

        fn bus(&self) -> &Self::Bus {
            &self.bus
        }

        fn device_type(&self) -> DeviceType {
            DeviceType::Block
        }

        fn reset(&self) {}

        fn status(&self) -> DeviceStatus {
            DeviceStatus::empty()
        }

        fn set_status(&self, _status: DeviceStatus) {}

        fn device_features(&self) -> u64 {
            0
        }

        fn set_driver_features(&self, _features: u64) {}

        fn queue_max_size(&self, _index: u16) -> u16 {
            8
        }

        fn set_queue(
            &self,
            _index: u16,
            _size: u16,
            _descriptor_area: u64,
            _driver_area: u64,
            _device_area: u64,
        ) {
        }

        fn notify_queue(&self, _index: u16) {}

        fn ack_interrupt(&self) {}

        fn read_config_u32(&self, _offset: usize) -> u32 {
            0
        }
    }

    unsafe impl Send for FakeDmaBuffer {}
    unsafe impl Sync for FakeDmaBuffer {}
    unsafe impl Send for FakeBus {}
    unsafe impl Sync for FakeBus {}

    #[test]
    fn queue_submit_and_pop_used_recycles_descriptors() {
        let transport = FakeTransport {
            bus: FakeBus { dma: FakeDmaPool },
        };
        let mut queue = VirtQueue::new(&transport, 0, 8).expect("queue should initialize");

        let input = [1u8; 16];
        let mut output = [0u8; 16];
        let token = queue
            .submit(&transport, &[&input], &mut [&mut output])
            .expect("submission should succeed");
        assert_eq!(token, 0);

        let desc_table = queue.descriptors.as_ptr().cast::<Descriptor>();
        let first = unsafe { desc_table.read() };
        let second = unsafe { desc_table.add(1).read() };
        assert_eq!(first.addr, input.as_ptr() as usize as u64);
        assert_eq!(first.flags & 1, 1);
        assert_eq!(second.addr, output.as_ptr() as usize as u64);
        assert_ne!(second.flags & 2, 0);

        let avail_idx = unsafe {
            queue
                .driver_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .read_volatile()
        };
        assert_eq!(avail_idx, 1);

        unsafe {
            queue
                .device_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .write_volatile(1);
            queue
                .device_area
                .as_ptr()
                .add(4)
                .cast::<UsedElem>()
                .write_volatile(UsedElem {
                    id: token as u32,
                    len: 0,
                });
        }

        assert_eq!(queue.pop_used(), Some(token));
        assert_eq!(queue.num_used, 0);
        assert_eq!(queue.free_head, 1);
    }

    #[test]
    fn deferred_submit_publishes_avail_index_once_for_batch() {
        let transport = FakeTransport {
            bus: FakeBus { dma: FakeDmaPool },
        };
        let mut queue = VirtQueue::new(&transport, 0, 8).expect("queue should initialize");
        let first = [1u8; 16];
        let second = [2u8; 16];

        let first_token = queue
            .submit_read_only_deferred(&transport, &first)
            .expect("first deferred submission should succeed");
        let second_token = queue
            .submit_read_only_deferred(&transport, &second)
            .expect("second deferred submission should succeed");

        let desc_table = queue.descriptors.as_ptr().cast::<Descriptor>();
        let first_desc = unsafe { desc_table.add(usize::from(first_token)).read() };
        let second_desc = unsafe { desc_table.add(usize::from(second_token)).read() };
        assert_eq!(first_desc.flags & 1, 0);
        assert_eq!(second_desc.flags & 1, 0);

        let unpublished_avail_idx = unsafe {
            queue
                .driver_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .read_volatile()
        };
        assert_eq!(unpublished_avail_idx, 0);

        queue.stage_deferred_head(first_token);
        queue.stage_deferred_head(second_token);
        queue.publish_deferred_heads();

        let published_avail_idx = unsafe {
            queue
                .driver_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .read_volatile()
        };
        assert_eq!(published_avail_idx, 2);
    }

    #[test]
    fn completed_descriptors_recycle_in_lifo_order() {
        let transport = FakeTransport {
            bus: FakeBus { dma: FakeDmaPool },
        };
        let mut queue = VirtQueue::new(&transport, 0, 8).expect("queue should initialize");
        let first_buffer = [1u8; 16];
        let second_buffer = [2u8; 16];

        let first_token = queue
            .submit(&transport, &[&first_buffer], &mut [])
            .expect("first submission should succeed");
        let second_token = queue
            .submit(&transport, &[&second_buffer], &mut [])
            .expect("second submission should succeed");

        unsafe {
            queue
                .device_area
                .as_ptr()
                .add(4)
                .cast::<UsedElem>()
                .write_volatile(UsedElem {
                    id: first_token as u32,
                    len: 0,
                });
            queue
                .device_area
                .as_ptr()
                .add(4 + size_of::<UsedElem>())
                .cast::<UsedElem>()
                .write_volatile(UsedElem {
                    id: second_token as u32,
                    len: 0,
                });
            queue
                .device_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .write_volatile(2);
        }

        assert_eq!(queue.pop_used(), Some(first_token));
        assert_eq!(queue.pop_used(), Some(second_token));
        assert_eq!(queue.next_free_descriptor(), second_token);
    }

    #[test]
    fn output_submit_can_reuse_specific_free_descriptor() {
        let transport = FakeTransport {
            bus: FakeBus { dma: FakeDmaPool },
        };
        let mut queue = VirtQueue::new(&transport, 0, 8).expect("queue should initialize");
        let first_buffer = [1u8; 16];
        let second_buffer = [2u8; 16];
        let mut output = [0u8; 16];

        let first_token = queue
            .submit(&transport, &[&first_buffer], &mut [])
            .expect("first submission should succeed");
        let second_token = queue
            .submit(&transport, &[&second_buffer], &mut [])
            .expect("second submission should succeed");

        unsafe {
            queue
                .device_area
                .as_ptr()
                .add(4)
                .cast::<UsedElem>()
                .write_volatile(UsedElem {
                    id: first_token as u32,
                    len: 0,
                });
            queue
                .device_area
                .as_ptr()
                .add(4 + size_of::<UsedElem>())
                .cast::<UsedElem>()
                .write_volatile(UsedElem {
                    id: second_token as u32,
                    len: 0,
                });
            queue
                .device_area
                .as_ptr()
                .cast::<u16>()
                .add(1)
                .write_volatile(2);
        }

        assert_eq!(queue.pop_used(), Some(first_token));
        assert_eq!(queue.pop_used(), Some(second_token));
        assert_eq!(queue.next_free_descriptor(), second_token);
        assert_eq!(
            queue
                .submit_output_at(&transport, first_token, &mut output)
                .expect("specific descriptor submit should succeed"),
            first_token
        );
        assert_eq!(queue.next_free_descriptor(), second_token);
    }

    #[test]
    fn used_ring_no_notify_flag_suppresses_queue_notify() {
        let transport = FakeTransport {
            bus: FakeBus { dma: FakeDmaPool },
        };
        let queue = VirtQueue::new(&transport, 0, 8).expect("queue should initialize");

        assert!(queue.should_notify());
        unsafe {
            queue
                .device_area
                .as_ptr()
                .cast::<u16>()
                .write_volatile(USED_FLAG_NO_NOTIFY);
        }
        assert!(!queue.should_notify());
    }
}
