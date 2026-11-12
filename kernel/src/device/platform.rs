//! The platform surface a granted device needs, as plain function
//! pointers.
//!
//! Same shape and reason as [`SwapVmHooks`](crate::SwapVmHooks) and the
//! runtime's memory hooks: the kernel is not generic over the address
//! space or the interrupt controller — there is exactly one of each per
//! machine, chosen at link time — and a `dyn` table would put a vtable
//! between a driver and its registers. The backend builds one `&'static`
//! table of each and installs it during bring-up, before it publishes
//! any grant.
//!
//! # Concurrency contract
//!
//! Both tables are written once, before secondary processors run, and
//! read from every processor afterwards. [`DeviceInterruptHooks::mask`]
//! is called from interrupt context and must therefore take no lock the
//! interrupted code could already hold; the memory hooks are called from
//! task context only. The four that change what backs a span —
//! substitute, restore, withdraw — are called only by the task that runs
//! the span's owner; `allocate_run` and `free_run` change no translation
//! and are called from any task.

use helios_hal::device::{DeviceRegion, DmaPlacement};
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpaceError, PageFlags, VirtAddr, VirtRange};
use spin::Once;

/// The address space's surface for buffers and the views of them.
pub struct DeviceVmHooks {
    /// Take a physically contiguous run of `bytes`, zeroed and mapped
    /// nowhere, placed as the device needs.
    ///
    /// Memory with no address of its own: what a device is told to read
    /// and what [`Self::substitute_memory`] shows an owner. It lives
    /// until [`Self::free_run`].
    pub allocate_run: fn(usize, DmaPlacement) -> Result<PhysicalRange, AddressSpaceError>,
    /// Give back a run [`Self::allocate_run`] produced.
    ///
    /// `align` is what the allocation asked for: a run is one
    /// allocation, and an allocator that was given a size and an
    /// alignment has to be given them back. Every substitution that
    /// showed the run has already ended.
    pub free_run: fn(PhysicalRange, u64) -> Result<(), AddressSpaceError>,
    /// Show a run at a span of an existing reservation whose pages are
    /// each committed or swapped out, in place of what is there, with
    /// the given flags and ordinary memory attributes.
    ///
    /// The caller is the one task that touches the span; every
    /// processor is shot down before the old frames are reusable.
    /// Nothing is allocated and nothing is charged: the run belongs to
    /// whoever allocated it.
    pub substitute_memory: fn(VirtRange, PhysicalRange, PageFlags) -> Result<(), AddressSpaceError>,
    /// Show a device's own frames at such a span, with the attributes
    /// the region carries.
    /// Everything [`Self::substitute_memory`] says holds.
    pub substitute_device: fn(VirtRange, DeviceRegion) -> Result<(), AddressSpaceError>,
    /// End a substitution and put zeroed anonymous memory back, with
    /// the permissions and owner the span had. What had been shown
    /// there is untouched.
    pub restore: fn(VirtRange) -> Result<(), AddressSpaceError>,
    /// End a substitution and leave the span reserved and uncommitted,
    /// shooting down every processor before returning. Allocates
    /// nothing, so it cannot fail for want of memory.
    pub withdraw: fn(VirtRange) -> Result<(), AddressSpaceError>,
    /// Where a physical frame appears in the kernel's own address
    /// space.
    ///
    /// A view of a run is in some owner's linear memory, which no
    /// device's DMA pool translates: every pool this kernel builds turns
    /// a pointer into a bus address by the fixed relation between
    /// physical memory and the kernel's own map. So a driver handed a
    /// slice — which is what the playback contract takes — has to be
    /// handed that alias, and the alias is the address a kernel-side
    /// producer writes the bytes through as well. Every backend has one:
    /// an offset map on x86 and aarch64, an identity map on riscv64, and
    /// the host's own address under `hosted`.
    pub kernel_alias: fn(PhysFrame) -> VirtAddr,
    /// The smallest unit at which this address space can change a
    /// mapping, in bytes.
    ///
    /// Never smaller than a [`PhysFrame`], and larger on a machine whose
    /// pages are: a run sized at a finer granularity than this would put
    /// two objects in one page, and showing either would show both.
    pub mapping_granule: fn() -> u64,
}

/// The interrupt controller's masking surface.
///
/// Completion is not here: a backend completes a source at its
/// controller on the interrupt path it already owns, and what a granted
/// device adds is only the ability to hold a source off until its owner
/// has serviced the device.
pub struct DeviceInterruptHooks {
    /// Stop the controller delivering this source. Called from
    /// interrupt context.
    pub mask: fn(u32),
    /// Let the controller deliver this source again.
    pub unmask: fn(u32),
}

static VM_HOOKS: Once<&'static DeviceVmHooks> = Once::new();
static INTERRUPT_HOOKS: Once<&'static DeviceInterruptHooks> = Once::new();

/// Install the backend's buffer and view surface. Called once during
/// bring-up, before any grant is published.
pub fn install_device_vm_hooks(hooks: &'static DeviceVmHooks) {
    let installed = VM_HOOKS.call_once(|| hooks);
    assert!(
        core::ptr::eq(*installed, hooks),
        "device virtual-memory hooks were installed more than once"
    );
}

/// Install the backend's interrupt-masking surface. Called once during
/// bring-up, before any grant is published.
pub fn install_device_interrupt_hooks(hooks: &'static DeviceInterruptHooks) {
    let installed = INTERRUPT_HOOKS.call_once(|| hooks);
    assert!(
        core::ptr::eq(*installed, hooks),
        "device interrupt hooks were installed more than once"
    );
}

/// The installed buffer and view surface.
///
/// # Panics
///
/// Panics when a buffer or a view is asked for on a backend that
/// installed no table. A backend that publishes a device, a display or
/// a sound card installs it first; reaching here means the bring-up
/// order is wrong, and mapping a register file without the backend's
/// attributes would be a silent corruption rather than a louder
/// failure.
pub(crate) fn device_vm_hooks() -> &'static DeviceVmHooks {
    VM_HOOKS
        .get()
        .copied()
        .expect("a buffer or a view was asked for before the backend installed its memory hooks")
}

/// The installed interrupt-masking surface. Panics for the same reason
/// as [`device_vm_hooks`].
pub(crate) fn device_interrupt_hooks() -> &'static DeviceInterruptHooks {
    INTERRUPT_HOOKS
        .get()
        .copied()
        .expect("a device grant was used before the backend installed its interrupt hooks")
}

/// Whether both tables are installed.
///
/// The registry checks this before it publishes anything, so a backend
/// that discovered devices without wiring the platform surface fails at
/// the publish rather than at the first driver's first register access.
pub(crate) fn device_hooks_installed() -> bool {
    VM_HOOKS.get().is_some() && INTERRUPT_HOOKS.get().is_some()
}

#[cfg(test)]
pub(crate) mod test_hooks {
    //! A recording platform surface for the kernel's own tests.
    //!
    //! The tables the kernel installs are `&'static` and write-once, so
    //! one recording pair serves the whole test binary. What they record
    //! is per-thread, because the test harness runs tests in parallel
    //! and a shared counter would make one test's assertion depend on
    //! another's timing. A test that hands a buffer to another thread
    //! has that thread's recording free it, so every test keeps its
    //! buffers on the thread that allocated them.

    use super::{DeviceInterruptHooks, DeviceVmHooks};
    use alloc::alloc::{Layout, alloc_zeroed, dealloc};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use helios_hal::device::{DeviceRegion, DeviceRegionAttributes, DmaPlacement};
    use helios_hal::iommu::PhysicalRange;
    use helios_hal::pmm::PhysFrame;
    use helios_hal::vmm::{AddressSpaceError, PageFlags, VirtAddr, VirtRange};

    /// One change the kernel asked the address space for.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum MappingChange {
        Allocated(PhysicalRange),
        Freed(PhysicalRange),
        SubstituteMemory(VirtRange, PhysicalRange),
        /// A device's own range shown at `VirtRange`, under the access
        /// rules it was shown with: what decides the memory type the
        /// page-table entries carry.
        SubstituteDevice(VirtRange, DeviceRegionAttributes),
        Restore(VirtRange),
        Withdraw(VirtRange),
    }

    #[derive(Default)]
    struct Recording {
        changes: Vec<MappingChange>,
        /// One per change of what backs a span, because every one of
        /// them ends in a shootdown: this is what a test asserts
        /// against. Allocating and freeing a run change no translation
        /// and are not counted.
        shootdowns: u64,
        masked: Vec<u32>,
        unmasked: Vec<u32>,
        /// Host memory standing in for the runs `allocate_run` hands
        /// out, keyed by their address.
        ///
        /// Real bytes rather than a bumped frame number, because
        /// [`DeviceVmHooks::kernel_alias`] is what a producer writes a
        /// period through and what a device is told to read: a test that
        /// drives the sample path dereferences this.
        allocated: Vec<(usize, *mut u8, Layout)>,
    }

    std::thread_local! {
        static RECORDING: RefCell<Recording> = RefCell::new(Recording::default());
    }

    fn record(change: MappingChange) {
        RECORDING.with(|recording| recording.borrow_mut().changes.push(change));
    }

    fn record_shootdown(change: MappingChange) {
        RECORDING.with(|recording| {
            let mut recording = recording.borrow_mut();
            recording.changes.push(change);
            recording.shootdowns += 1;
        });
    }

    fn allocate_run(
        bytes: usize,
        placement: DmaPlacement,
    ) -> Result<PhysicalRange, AddressSpaceError> {
        let align = usize::try_from(placement.align)
            .unwrap_or(PhysFrame::SIZE)
            .max(PhysFrame::SIZE);
        let layout =
            Layout::from_size_align(bytes, align).map_err(|_| AddressSpaceError::Misaligned)?;
        // SAFETY: the layout has a non-zero size, because a run of no
        // bytes is refused before it reaches an address space.
        let run = unsafe { alloc_zeroed(layout) };
        if run.is_null() {
            return Err(AddressSpaceError::OutOfFrames);
        }
        if !placement.accepts(run as usize as u64, bytes as u64) {
            // SAFETY: `run` came from `alloc_zeroed` with this layout
            // and nothing has been recorded against it.
            unsafe { dealloc(run, layout) };
            return Err(AddressSpaceError::OutOfFrames);
        }
        let physical = PhysicalRange::new(run as usize as u64, bytes as u64);
        RECORDING.with(|recording| {
            recording
                .borrow_mut()
                .allocated
                .push((run as usize, run, layout));
        });
        record(MappingChange::Allocated(physical));
        Ok(physical)
    }

    fn free_run(run: PhysicalRange, _align: u64) -> Result<(), AddressSpaceError> {
        let held = RECORDING.with(|recording| {
            let mut recording = recording.borrow_mut();
            let index = recording
                .allocated
                .iter()
                .position(|(start, _, _)| *start as u64 == run.start)?;
            Some(recording.allocated.remove(index))
        });
        let (_, memory, layout) = held.ok_or(AddressSpaceError::NotCommitted)?;
        // SAFETY: `memory` came from this module's own `alloc_zeroed`
        // with `layout`, and it is removed from the list before it is
        // freed, so nothing frees it twice.
        unsafe { dealloc(memory, layout) };
        record(MappingChange::Freed(run));
        Ok(())
    }

    fn substitute_memory(
        virt: VirtRange,
        physical: PhysicalRange,
        _flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        if physical.bytes as usize != virt.byte_len {
            return Err(AddressSpaceError::Misaligned);
        }
        record_shootdown(MappingChange::SubstituteMemory(virt, physical));
        Ok(())
    }

    fn substitute_device(virt: VirtRange, region: DeviceRegion) -> Result<(), AddressSpaceError> {
        if region.physical.bytes as usize != virt.byte_len {
            return Err(AddressSpaceError::Misaligned);
        }
        record_shootdown(MappingChange::SubstituteDevice(virt, region.attributes));
        Ok(())
    }

    fn restore(virt: VirtRange) -> Result<(), AddressSpaceError> {
        record_shootdown(MappingChange::Restore(virt));
        Ok(())
    }

    fn withdraw(virt: VirtRange) -> Result<(), AddressSpaceError> {
        record_shootdown(MappingChange::Withdraw(virt));
        Ok(())
    }

    fn mask(source: u32) {
        RECORDING.with(|recording| recording.borrow_mut().masked.push(source));
    }

    fn unmask(source: u32) {
        RECORDING.with(|recording| recording.borrow_mut().unmasked.push(source));
    }

    fn mapping_granule() -> u64 {
        PhysFrame::SIZE as u64
    }

    /// A run's "physical" address under these tables is the host
    /// allocation behind it, so the kernel's alias for it is itself.
    fn kernel_alias(frame: PhysFrame) -> VirtAddr {
        VirtAddr::new(frame.phys_addr())
    }

    static VM: DeviceVmHooks = DeviceVmHooks {
        allocate_run,
        free_run,
        substitute_memory,
        substitute_device,
        restore,
        withdraw,
        mapping_granule,
        kernel_alias,
    };

    static INTERRUPTS: DeviceInterruptHooks = DeviceInterruptHooks { mask, unmask };

    /// Install the recording tables, idempotently.
    pub fn install() {
        super::VM_HOOKS.call_once(|| &VM);
        super::INTERRUPT_HOOKS.call_once(|| &INTERRUPTS);
    }

    /// Shootdowns this test's thread has issued.
    pub fn shootdowns() -> u64 {
        RECORDING.with(|recording| recording.borrow().shootdowns)
    }

    /// The changes this test's thread has recorded.
    pub fn changes() -> Vec<MappingChange> {
        RECORDING.with(|recording| recording.borrow().changes.clone())
    }

    /// Runs this test's thread has allocated and not freed.
    pub fn live_runs() -> usize {
        RECORDING.with(|recording| recording.borrow().allocated.len())
    }

    /// The sources this test's thread has held off at the controller.
    pub fn masked() -> Vec<u32> {
        RECORDING.with(|recording| recording.borrow().masked.clone())
    }

    /// The sources this test's thread has let through again.
    pub fn unmasked() -> Vec<u32> {
        RECORDING.with(|recording| recording.borrow().unmasked.clone())
    }
}
