//! The `Notify` operations AML executes, collected for the host.
//!
//! The interpreter calls [`Handler::handle_notify`] in the middle of a
//! method, where the host can do nothing but remember it: the method has
//! the namespace and possibly an AML mutex in hand. [`NotifyRecorder`]
//! wraps the backend's handler, forwards every other operation to it, and
//! queues each notification for the code that ran the method to collect
//! once it returns.
//!
//! Concurrency contract: the recorder and every clone of it belong to one
//! interpreter, which has one thread of control (`namespace`'s contract);
//! the queue is a `RefCell` behind an `Rc` and the recorder is neither
//! `Send` nor `Sync`.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use acpi::aml::AmlError;
use acpi::aml::namespace::AmlName;
use acpi::aml::object::Object;
use acpi::{Handle, Handler, PciAddress, RawPhysicalMapping};

/// One `Notify (object, value)` the interpreter executed.
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    /// The absolute path of the notified device, processor or thermal
    /// zone.
    pub object: AmlName,
    pub value: u64,
}

/// A [`Handler`] that queues every notification and forwards everything
/// else to `H`.
#[derive(Clone)]
pub struct NotifyRecorder<H> {
    inner: H,
    queued: Rc<RefCell<Vec<Notification>>>,
}

impl<H> NotifyRecorder<H> {
    pub fn new(inner: H) -> Self {
        Self {
            inner,
            queued: Rc::default(),
        }
    }

    /// The notifications executed since the previous call, in order.
    pub fn take(&self) -> Vec<Notification> {
        core::mem::take(&mut *self.queued.borrow_mut())
    }

    /// The handler every other operation goes to.
    pub fn inner(&self) -> &H {
        &self.inner
    }
}

impl<H: Handler> Handler for NotifyRecorder<H> {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> RawPhysicalMapping<T> {
        // SAFETY: forwarded under the caller's contract.
        unsafe { self.inner.map_physical_region(physical_address, size) }
    }

    unsafe fn unmap_physical_region<T>(&self, region: RawPhysicalMapping<T>) {
        // SAFETY: forwarded under the caller's contract.
        unsafe { self.inner.unmap_physical_region(region) }
    }

    fn read_u8(&self, address: usize) -> u8 {
        self.inner.read_u8(address)
    }

    fn read_u16(&self, address: usize) -> u16 {
        self.inner.read_u16(address)
    }

    fn read_u32(&self, address: usize) -> u32 {
        self.inner.read_u32(address)
    }

    fn read_u64(&self, address: usize) -> u64 {
        self.inner.read_u64(address)
    }

    fn write_u8(&self, address: usize, value: u8) {
        self.inner.write_u8(address, value)
    }

    fn write_u16(&self, address: usize, value: u16) {
        self.inner.write_u16(address, value)
    }

    fn write_u32(&self, address: usize, value: u32) {
        self.inner.write_u32(address, value)
    }

    fn write_u64(&self, address: usize, value: u64) {
        self.inner.write_u64(address, value)
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        self.inner.read_io_u8(port)
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        self.inner.read_io_u16(port)
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        self.inner.read_io_u32(port)
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        self.inner.write_io_u8(port, value)
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        self.inner.write_io_u16(port, value)
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        self.inner.write_io_u32(port, value)
    }

    fn read_pci_u8(&self, address: PciAddress, offset: u16) -> u8 {
        self.inner.read_pci_u8(address, offset)
    }

    fn read_pci_u16(&self, address: PciAddress, offset: u16) -> u16 {
        self.inner.read_pci_u16(address, offset)
    }

    fn read_pci_u32(&self, address: PciAddress, offset: u16) -> u32 {
        self.inner.read_pci_u32(address, offset)
    }

    fn write_pci_u8(&self, address: PciAddress, offset: u16, value: u8) {
        self.inner.write_pci_u8(address, offset, value)
    }

    fn write_pci_u16(&self, address: PciAddress, offset: u16, value: u16) {
        self.inner.write_pci_u16(address, offset, value)
    }

    fn write_pci_u32(&self, address: PciAddress, offset: u16, value: u32) {
        self.inner.write_pci_u32(address, offset, value)
    }

    fn nanos_since_boot(&self) -> u64 {
        self.inner.nanos_since_boot()
    }

    fn stall(&self, microseconds: u64) {
        self.inner.stall(microseconds)
    }

    fn sleep(&self, milliseconds: u64) {
        self.inner.sleep(milliseconds)
    }

    fn create_mutex(&self) -> Handle {
        self.inner.create_mutex()
    }

    fn acquire(&self, mutex: Handle, timeout: u16) -> Result<(), AmlError> {
        self.inner.acquire(mutex, timeout)
    }

    fn release(&self, mutex: Handle) {
        self.inner.release(mutex)
    }

    fn breakpoint(&self) {
        self.inner.breakpoint()
    }

    fn handle_debug(&self, object: &Object) {
        self.inner.handle_debug(object)
    }

    fn handle_fatal_error(&self, fatal_type: u8, fatal_code: u32, fatal_arg: u64) {
        self.inner
            .handle_fatal_error(fatal_type, fatal_code, fatal_arg)
    }

    fn handle_notify(&self, object: &AmlName, value: u64) {
        self.queued.borrow_mut().push(Notification {
            object: object.clone(),
            value,
        });
    }
}
