//! GICv3 bring-up for the AArch64 backend.
//!
//! Concurrency contract: every distributor and redistributor write goes
//! through [`Gic`]'s spin lock, and all of them happen during bring-up —
//! the bootstrap processor configures the distributor and the device
//! interrupts, each processor initialises its own CPU interface as it
//! comes online. The interrupt path itself never takes the lock:
//! acknowledging and ending an interrupt are `ICC_*_EL1` system-register
//! accesses private to the running processor.

use core::ptr::NonNull;

use arm_gic::gicv3::registers::{Gicd, GicrSgi};
use arm_gic::gicv3::{GicCpuInterface, GicV3, SgiTarget, SgiTargetGroup};
use arm_gic::{IntId, InterruptGroup, Trigger, UniqueMmioPointer};
use spin::Mutex;

use crate::platform::GicDescription;

/// The kernel runs at non-secure EL1, where group 1 interrupts are the
/// ones signalled as IRQ.
const GROUP: InterruptGroup = InterruptGroup::Group1;

/// `CNTV` — the EL1 virtual timer the kernel arms through
/// `cntv_cval_el0` — is private peripheral interrupt 11, INTID 27.
pub(crate) const VIRTUAL_TIMER_PPI: IntId = IntId::ppi(11);

/// Software-generated interrupt that pulls one specific processor out
/// of `wfi`. It carries no payload: taking it is the whole message.
pub(crate) const WAKE_SGI: IntId = IntId::sgi(0);

/// Lowest priority value the CPU interface accepts, so every interrupt
/// the distributor signals reaches the processor. The driver's default
/// settings give each interrupt the highest non-secure priority.
const PRIORITY_MASK_ALL: u8 = 0xff;

/// Affinity bits of `MPIDR_EL1`: Aff0..Aff2 in the low word and Aff3 in
/// bits 32..40. `GICR_TYPER.Affinity` is reported in the same shape.
const MPIDR_AFFINITY_MASK: u64 = 0x0000_00ff_00ff_ffff;

/// The platform's interrupt controller.
pub(crate) struct Gic {
    inner: Mutex<GicV3<'static>>,
    processor_count: usize,
}

impl Gic {
    /// Maps the controller, configures the distributor, and brings up
    /// the bootstrap processor's CPU interface.
    ///
    /// Every interrupt is left disabled: the kernel enables exactly the
    /// timer PPI on each processor and the device SPIs it has routes
    /// for, so an interrupt the firmware left armed fails loudly in the
    /// dispatcher instead of being silently acknowledged.
    pub(crate) fn new(
        description: &GicDescription,
        processor_mpidrs: impl Iterator<Item = u64> + Clone,
        bootstrap_mpidr: u64,
        physical_memory_offset: usize,
        handoff: &crate::LimineBootHandoff,
    ) -> Self {
        let processor_count = processor_mpidrs.clone().count();
        description.check_covers(processor_mpidrs);
        for region in [description.distributor, description.redistributor] {
            crate::map_mmio_range(region.base, region.size, physical_memory_offset, handoff);
        }
        let distributor = NonNull::new(crate::mmio_virtual_base(
            description.distributor.base,
            physical_memory_offset,
        ) as *mut Gicd)
        .unwrap_or_else(|| panic!("AArch64 GIC distributor mapped to a null address"));
        let redistributor = NonNull::new(crate::mmio_virtual_base(
            description.redistributor.base,
            physical_memory_offset,
        ) as *mut GicrSgi)
        .unwrap_or_else(|| panic!("AArch64 GIC redistributor mapped to a null address"));
        // SAFETY: both windows were just mapped as device memory from
        // the platform's own description, and this is the only driver
        // instance the kernel constructs.
        let distributor = unsafe { UniqueMmioPointer::new(distributor) };
        let mut gic = unsafe { GicV3::new(distributor, redistributor, processor_count) }
            .unwrap_or_else(|error| panic!("AArch64 GICv3 initialisation failed: {error}"));
        let bootstrap = redistributor_index(&mut gic, bootstrap_mpidr, processor_count);
        gic.setup(bootstrap);
        gic.enable_all_interrupts(false);
        tracing::info!(
            "GICv3 online gicd={:#x} gicr={:#x} processors={processor_count} bootstrap_redistributor={bootstrap}",
            description.distributor.base,
            description.redistributor.base,
        );
        Self {
            inner: Mutex::new(gic),
            processor_count,
        }
    }

    /// Initialises the calling processor's CPU interface and enables the
    /// interrupts private to it.
    pub(crate) fn attach_current_processor(&self, mpidr: u64) {
        let mut gic = self.inner.lock();
        let index = redistributor_index(&mut gic, mpidr, self.processor_count);
        gic.init_cpu(index);
        GicCpuInterface::enable_group1(true);
        GicCpuInterface::set_priority_mask(PRIORITY_MASK_ALL);
        for intid in [VIRTUAL_TIMER_PPI, WAKE_SGI] {
            gic.enable_interrupt(intid, Some(index), true)
                .unwrap_or_else(|error| {
                    panic!(
                        "AArch64 GIC could not enable {intid:?} on redistributor {index}: {error}"
                    )
                });
        }
        tracing::debug!("GICv3 cpu interface online mpidr={mpidr:#x} redistributor={index}");
    }

    /// Routes a device interrupt to the processor with `mpidr` and
    /// enables it with the trigger mode the device tree declared.
    pub(crate) fn enable_device_interrupt(&self, intid: IntId, trigger: Trigger, mpidr: u64) {
        let mut gic = self.inner.lock();
        gic.distributor()
            .set_routing(intid, Some(mpidr & MPIDR_AFFINITY_MASK))
            .unwrap_or_else(|error| panic!("AArch64 GIC could not route {intid:?}: {error}"));
        gic.set_trigger(intid, None, trigger)
            .unwrap_or_else(|error| {
                panic!("AArch64 GIC could not set the trigger of {intid:?}: {error}")
            });
        gic.enable_interrupt(intid, None, true)
            .unwrap_or_else(|error| panic!("AArch64 GIC could not enable {intid:?}: {error}"));
        tracing::info!("GICv3 routed {intid:?} to mpidr={mpidr:#x} trigger={trigger:?}");
    }
}

/// Sends the wake SGI to the single processor with `mpidr`.
///
/// Unlike the `sev` this replaces, the interrupt reaches only its
/// target, so waking one task no longer drags every parked processor
/// out of `wfi`.
pub(crate) fn send_wake(mpidr: u64) {
    let affinity0 = (mpidr & 0xff) as u8;
    assert!(
        affinity0 < 16,
        "AArch64 processor MPIDR {mpidr:#x} has an affinity 0 outside a single SGI target list"
    );
    GicCpuInterface::send_sgi(
        WAKE_SGI,
        SgiTarget::List {
            affinity3: ((mpidr >> 32) & 0xff) as u8,
            affinity2: ((mpidr >> 16) & 0xff) as u8,
            affinity1: ((mpidr >> 8) & 0xff) as u8,
            target_list: 1 << affinity0,
        },
        SgiTargetGroup::CurrentGroup1,
    )
    .unwrap_or_else(|error| panic!("AArch64 GIC could not send the wake SGI: {error}"));
}

/// Acknowledges the highest priority pending interrupt, if any.
pub(crate) fn acknowledge_interrupt() -> Option<IntId> {
    GicCpuInterface::get_and_acknowledge_interrupt(GROUP)
}

/// Drops the priority of an acknowledged interrupt and deactivates it.
pub(crate) fn end_interrupt(intid: IntId) {
    GicCpuInterface::end_interrupt(intid, GROUP);
}

/// Finds the redistributor frame belonging to the processor with
/// `mpidr`. Redistributor order is implementation defined, so the frame
/// is matched by the affinity each one reports rather than assumed to
/// follow the boot processor order.
fn redistributor_index(gic: &mut GicV3<'static>, mpidr: u64, processor_count: usize) -> usize {
    let affinity = mpidr & MPIDR_AFFINITY_MASK;
    (0..processor_count)
        .find(|&index| {
            gic.gicr_typer(index)
                .unwrap_or_else(|error| {
                    panic!("AArch64 GIC redistributor {index} is unavailable: {error}")
                })
                .core_mpidr()
                == affinity
        })
        .unwrap_or_else(|| panic!("AArch64 GIC has no redistributor for MPIDR {mpidr:#x}"))
}
