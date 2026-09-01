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
use arm_gic::gicv3::{GicCpuInterface, GicV3};
use arm_gic::{IntId, InterruptGroup, Trigger, UniqueMmioPointer};
use fdt::Fdt;
use spin::Mutex;

/// The kernel runs at non-secure EL1, where group 1 interrupts are the
/// ones signalled as IRQ.
const GROUP: InterruptGroup = InterruptGroup::Group1;

/// `CNTV` — the EL1 virtual timer the kernel arms through
/// `cntv_cval_el0` — is private peripheral interrupt 11, INTID 27.
pub(crate) const VIRTUAL_TIMER_PPI: IntId = IntId::ppi(11);

/// Lowest priority value the CPU interface accepts, so every interrupt
/// the distributor signals reaches the processor. The driver's default
/// settings give each interrupt the highest non-secure priority.
const PRIORITY_MASK_ALL: u8 = 0xff;

/// Affinity bits of `MPIDR_EL1`: Aff0..Aff2 in the low word and Aff3 in
/// bits 32..40. `GICR_TYPER.Affinity` is reported in the same shape.
const MPIDR_AFFINITY_MASK: u64 = 0x0000_00ff_00ff_ffff;

/// Physical layout of the GICv3 register frames, as the device tree
/// describes them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GicRegions {
    distributor: usize,
    distributor_bytes: usize,
    redistributor: usize,
    redistributor_bytes: usize,
}

impl GicRegions {
    /// Reads the distributor and redistributor windows from the
    /// `arm,gic-v3` interrupt controller node.
    ///
    /// The redistributor window covers one frame pair per processor, so
    /// the driver walks it with the stride the frames themselves report.
    pub(crate) fn discover(fdt: &Fdt<'_>) -> Self {
        let node = fdt
            .all_nodes()
            .find(|node| {
                node.compatible()
                    .is_some_and(|compatible| compatible.all().any(|entry| entry == "arm,gic-v3"))
            })
            .unwrap_or_else(|| {
                panic!("AArch64 device tree does not describe an arm,gic-v3 interrupt controller")
            });
        if let Some(regions) = node.property("#redistributor-regions") {
            let regions =
                crate::fdt_cells_to_usize(regions.value, "AArch64 GIC #redistributor-regions");
            assert!(
                regions == 1,
                "AArch64 GIC declares {regions} redistributor regions, only one is supported"
            );
        }
        let mut reg = node
            .raw_reg()
            .unwrap_or_else(|| panic!("AArch64 GIC node has no reg property"));
        let distributor = reg
            .next()
            .unwrap_or_else(|| panic!("AArch64 GIC node has no distributor reg entry"));
        let redistributor = reg
            .next()
            .unwrap_or_else(|| panic!("AArch64 GIC node has no redistributor reg entry"));
        Self {
            distributor: crate::fdt_cells_to_usize(distributor.address, "AArch64 GICD reg address"),
            distributor_bytes: crate::fdt_cells_to_usize(distributor.size, "AArch64 GICD reg size"),
            redistributor: crate::fdt_cells_to_usize(
                redistributor.address,
                "AArch64 GICR reg address",
            ),
            redistributor_bytes: crate::fdt_cells_to_usize(
                redistributor.size,
                "AArch64 GICR reg size",
            ),
        }
    }
}

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
        regions: GicRegions,
        processor_count: usize,
        bootstrap_mpidr: u64,
        physical_memory_offset: usize,
        handoff: &crate::LimineBootHandoff,
    ) -> Self {
        crate::map_mmio_range(
            regions.distributor,
            regions.distributor_bytes,
            physical_memory_offset,
            handoff,
        );
        crate::map_mmio_range(
            regions.redistributor,
            regions.redistributor_bytes,
            physical_memory_offset,
            handoff,
        );
        let distributor = NonNull::new(crate::mmio_virtual_base(
            regions.distributor,
            physical_memory_offset,
        ) as *mut Gicd)
        .unwrap_or_else(|| panic!("AArch64 GIC distributor mapped to a null address"));
        let redistributor = NonNull::new(crate::mmio_virtual_base(
            regions.redistributor,
            physical_memory_offset,
        ) as *mut GicrSgi)
        .unwrap_or_else(|| panic!("AArch64 GIC redistributor mapped to a null address"));
        // SAFETY: both windows were just mapped as device memory from
        // the device tree's own description, and this is the only
        // driver instance the kernel constructs.
        let distributor = unsafe { UniqueMmioPointer::new(distributor) };
        let mut gic = unsafe { GicV3::new(distributor, redistributor, processor_count) }
            .unwrap_or_else(|error| panic!("AArch64 GICv3 initialisation failed: {error}"));
        let bootstrap = redistributor_index(&mut gic, bootstrap_mpidr, processor_count);
        gic.setup(bootstrap);
        gic.enable_all_interrupts(false);
        tracing::info!(
            "GICv3 online gicd={:#x} gicr={:#x} processors={processor_count} bootstrap_redistributor={bootstrap}",
            regions.distributor,
            regions.redistributor,
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
        gic.enable_interrupt(VIRTUAL_TIMER_PPI, Some(index), true)
            .unwrap_or_else(|error| {
                panic!("AArch64 GIC could not enable the virtual timer PPI: {error}")
            });
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

/// Acknowledges the highest priority pending interrupt, if any.
pub(crate) fn acknowledge_interrupt() -> Option<IntId> {
    GicCpuInterface::get_and_acknowledge_interrupt(GROUP)
}

/// Drops the priority of an acknowledged interrupt and deactivates it.
pub(crate) fn end_interrupt(intid: IntId) {
    GicCpuInterface::end_interrupt(intid, GROUP);
}

/// Resolves the INTID and trigger mode of the shared peripheral
/// interrupt a `virtio,mmio` node declares. A device the kernel drives
/// must name one: without it the driver would have no way to learn about
/// completions other than polling.
pub(crate) fn device_interrupt(
    interrupt: Option<helios_virtio::MmioInterrupt>,
    base: usize,
) -> (IntId, Trigger) {
    let interrupt = interrupt.unwrap_or_else(|| {
        panic!("virtio MMIO device at {base:#x} declares no interrupt in the device tree")
    });
    let trigger = interrupt.trigger.unwrap_or_else(|| {
        panic!("virtio MMIO device at {base:#x} declares no interrupt trigger mode")
    });
    let trigger = match trigger {
        helios_virtio::InterruptTrigger::Edge => Trigger::Edge,
        helios_virtio::InterruptTrigger::Level => Trigger::Level,
    };
    (IntId::spi(interrupt.number), trigger)
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
