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
//! task context only.

use helios_hal::device::{DeviceRegion, DmaPlacement};
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpaceError, PageFlags, VirtAddr, VirtRange};
use spin::Once;

/// The address space's device-mapping surface.
pub struct DeviceVmHooks {
    /// Map a device's own frames at a range of an existing reservation,
    /// with the attributes the region carries.
    pub map_device: fn(VirtRange, DeviceRegion) -> Result<(), AddressSpaceError>,
    /// Drop a mapping [`Self::map_device`] installed, shooting down
    /// every processor before returning.
    pub unmap_device: fn(VirtRange) -> Result<(), AddressSpaceError>,
    /// Commit physically contiguous backing no byte of which sits above
    /// `limit`, and report its first frame.
    pub commit_contiguous:
        fn(VirtRange, PageFlags, DmaPlacement) -> Result<PhysFrame, AddressSpaceError>,
    /// Map ordinary memory another instance already committed at a
    /// range of an existing reservation, so both hold the same bytes.
    ///
    /// Nothing is allocated and nothing is accounted for: the run
    /// belongs to whoever committed it, and this is the second view of
    /// it.
    pub map_shared: fn(VirtRange, PhysicalRange, PageFlags) -> Result<(), AddressSpaceError>,
    /// Drop a mapping [`Self::map_shared`] installed, shooting down
    /// every processor before returning. The run itself is untouched.
    pub unmap_shared: fn(VirtRange) -> Result<(), AddressSpaceError>,
    /// Release backing [`Self::commit_contiguous`] installed.
    ///
    /// `align` is what the matching commit asked for. A contiguous run
    /// is one allocation, not a pile of frames, and an allocator that
    /// was given a size and an alignment has to be given them back.
    pub release_contiguous: fn(VirtRange, u64) -> Result<(), AddressSpaceError>,
    /// Where a physical frame appears in the kernel's own address
    /// space.
    ///
    /// The mapping [`Self::commit_contiguous`] installs is in the
    /// owner's linear memory, which is a window no device's DMA pool
    /// translates: every pool this kernel builds turns a pointer into a
    /// bus address by the fixed relation between physical memory and
    /// the kernel's own map. So a driver handed a slice — which is what
    /// the playback contract takes — has to be handed that alias, and
    /// the alias is the address a kernel-side producer writes the bytes
    /// through as well. Every backend has one: an offset map on x86 and
    /// aarch64, an identity map on riscv64, and the host's own address
    /// under `hosted`.
    pub kernel_alias: fn(PhysFrame) -> VirtAddr,
    /// The smallest unit at which this address space can change a
    /// mapping, in bytes.
    ///
    /// Never smaller than a [`PhysFrame`], and larger on a machine whose
    /// pages are: a device window carved at a finer granularity than
    /// this would put two regions in one page, and changing either would
    /// change both.
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

/// Install the backend's device-mapping surface. Called once during
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

/// The installed device-mapping surface.
///
/// # Panics
///
/// Panics when a grant is used on a backend that installed no table. A
/// grant is only published by a backend that discovered a device, and a
/// backend that can discover one can map it; reaching here means the
/// bring-up order is wrong, and mapping a register file without the
/// backend's attributes would be a silent corruption rather than a
/// louder failure.
pub(crate) fn device_vm_hooks() -> &'static DeviceVmHooks {
    VM_HOOKS
        .get()
        .copied()
        .expect("a device grant was used before the backend installed its memory hooks")
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
    //! another's timing.

    use super::{DeviceInterruptHooks, DeviceVmHooks};
    use alloc::alloc::{Layout, alloc_zeroed, dealloc};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use helios_hal::device::{DeviceRegion, DmaPlacement};
    use helios_hal::iommu::PhysicalRange;
    use helios_hal::pmm::PhysFrame;
    use helios_hal::vmm::{AddressSpaceError, PageFlags, VirtAddr, VirtRange};

    /// One mapping change the kernel asked the address space for.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum MappingChange {
        MapDevice(VirtRange),
        UnmapDevice(VirtRange),
        Commit(VirtRange),
        Released(VirtRange),
        MapShared(VirtRange, PhysicalRange),
        UnmapShared(VirtRange),
    }

    #[derive(Default)]
    struct Recording {
        changes: Vec<MappingChange>,
        /// One per mapping change, because every one of them ends in a
        /// shootdown: this is what a test asserts against.
        shootdowns: u64,
        masked: Vec<u32>,
        unmasked: Vec<u32>,
        /// Host memory standing in for the physical runs a commit hands
        /// out, keyed by the range each was committed at.
        ///
        /// Real bytes rather than a bumped frame number, because
        /// [`DeviceVmHooks::kernel_alias`] is what a producer writes a
        /// period through and what a device is told to read: a test that
        /// drives the sample path dereferences this.
        runs: Vec<(usize, *mut u8, Layout)>,
    }

    std::thread_local! {
        static RECORDING: RefCell<Recording> = RefCell::new(Recording::default());
    }

    fn record(change: MappingChange) {
        RECORDING.with(|recording| {
            let mut recording = recording.borrow_mut();
            recording.changes.push(change);
            recording.shootdowns += 1;
        });
    }

    fn map_device(virt: VirtRange, _region: DeviceRegion) -> Result<(), AddressSpaceError> {
        record(MappingChange::MapDevice(virt));
        Ok(())
    }

    fn unmap_device(virt: VirtRange) -> Result<(), AddressSpaceError> {
        record(MappingChange::UnmapDevice(virt));
        Ok(())
    }

    fn commit_contiguous(
        virt: VirtRange,
        _flags: PageFlags,
        placement: DmaPlacement,
    ) -> Result<PhysFrame, AddressSpaceError> {
        let align = usize::try_from(placement.align)
            .unwrap_or(PhysFrame::SIZE)
            .max(PhysFrame::SIZE);
        let layout = Layout::from_size_align(virt.byte_len, align)
            .map_err(|_| AddressSpaceError::Misaligned)?;
        // SAFETY: the layout has a non-zero size, because a commit of no
        // bytes is refused before it reaches an address space.
        let run = unsafe { alloc_zeroed(layout) };
        if run.is_null() {
            return Err(AddressSpaceError::OutOfFrames);
        }
        let frame = PhysFrame::from_phys_addr(run as usize);
        if !placement.accepts(frame.phys_addr() as u64, virt.byte_len as u64) {
            // SAFETY: `run` came from `alloc_zeroed` with this layout
            // and nothing has been recorded against it.
            unsafe { dealloc(run, layout) };
            return Err(AddressSpaceError::OutOfFrames);
        }
        RECORDING.with(|recording| {
            recording
                .borrow_mut()
                .runs
                .push((virt.start.raw(), run, layout));
        });
        record(MappingChange::Commit(virt));
        Ok(frame)
    }

    fn release_contiguous(virt: VirtRange, _align: u64) -> Result<(), AddressSpaceError> {
        let held = RECORDING.with(|recording| {
            let mut recording = recording.borrow_mut();
            let index = recording
                .runs
                .iter()
                .position(|(start, _, _)| *start == virt.start.raw())?;
            Some(recording.runs.remove(index))
        });
        if let Some((_, run, layout)) = held {
            // SAFETY: `run` came from this module's own `alloc_zeroed`
            // with `layout`, and it is removed from the list before it
            // is freed, so nothing frees it twice.
            unsafe { dealloc(run, layout) };
        }
        record(MappingChange::Released(virt));
        Ok(())
    }

    fn map_shared(
        virt: VirtRange,
        physical: PhysicalRange,
        _flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        if physical.bytes as usize != virt.byte_len {
            return Err(AddressSpaceError::Misaligned);
        }
        record(MappingChange::MapShared(virt, physical));
        Ok(())
    }

    fn unmap_shared(virt: VirtRange) -> Result<(), AddressSpaceError> {
        record(MappingChange::UnmapShared(virt));
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

    /// A commit's "physical" address under these tables is the host
    /// allocation behind it, so the kernel's alias for it is itself.
    fn kernel_alias(frame: PhysFrame) -> VirtAddr {
        VirtAddr::new(frame.phys_addr())
    }

    static VM: DeviceVmHooks = DeviceVmHooks {
        map_device,
        unmap_device,
        map_shared,
        unmap_shared,
        commit_contiguous,
        release_contiguous,
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

    /// The mapping changes this test's thread has recorded.
    pub fn changes() -> Vec<MappingChange> {
        RECORDING.with(|recording| recording.borrow().changes.clone())
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
