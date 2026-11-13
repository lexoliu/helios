//! The machine's TPM 2.0, as the platform description names it, handed
//! to the kernel as its measured-boot device.
//!
//! Adapter code only: the description (`platform`) says where locality
//! 0 is and which interface it is, this module maps the page, and
//! `helios_tpm` opens the interface and runs the checks that decide
//! whether the kernel may use the TPM. Both firmware descriptions
//! arrive here the same way: a device tree's `tcg,tpm-tis-mmio` node is
//! the FIFO interface, and an ACPI TPM2 table names its interface by
//! start method.
//!
//! Concurrency contract: discovery runs once, on the bootstrap
//! processor, before the kernel is initialised and before any secondary
//! starts. The device it yields is owned by the kernel's bring-up.

use core::ptr::NonNull;

use helios_hal::boot::MeasurementLog;
use helios_tpm::{
    Discovered, Discovery, InterfaceError, LOCALITY_BYTES, LoaderLog, MmioWindow, Unqualified,
};

use crate::platform::{PlatformDescription, TpmDescription};
use crate::{Aarch64Cpu, LimineBootHandoff};

/// Opens the TPM the platform describes and runs the measured-boot
/// checks against it.
///
/// A TPM that fails a check is reported and not handed over; one that
/// fails while it is checked stops the boot.
pub(crate) fn discover(
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &LimineBootHandoff,
    cpu: Aarch64Cpu,
) -> Discovered<MmioWindow, Aarch64Cpu> {
    let (base, interface) = match platform.tpm {
        None => return (None, Discovery::NotDescribed),
        Some(TpmDescription::Unsupported { start_method }) => {
            return (
                None,
                Discovery::Refused {
                    base: None,
                    reason: Unqualified::Interface(InterfaceError::StartMethod(start_method)),
                },
            );
        }
        Some(TpmDescription::Described { base, interface }) => (base, interface),
    };
    let physical_base = usize::try_from(base)
        .unwrap_or_else(|_| panic!("the TPM at {base:#x} is outside the physical address space"));
    crate::map_mmio_range(
        physical_base,
        LOCALITY_BYTES,
        physical_memory_offset,
        handoff,
    );
    let virtual_base = crate::mmio_virtual_base(physical_base, physical_memory_offset);
    // SAFETY: `map_mmio_range` mapped locality 0's page as device memory
    // and never unmaps it, and nothing else in this kernel touches the
    // TPM's registers.
    let window = unsafe {
        MmioWindow::new(
            NonNull::new(virtual_base as *mut u8)
                .unwrap_or_else(|| panic!("the TPM's register page mapped to a null address")),
            LOCALITY_BYTES,
        )
    };
    let log = handoff
        .tables
        .measurement_log
        .as_ref()
        // SAFETY: Limine's event log lives in bootloader-reclaimable
        // memory, which this backend never hands to the allocator, in the
        // direct map the response points into.
        .map(|log: &MeasurementLog| unsafe { LoaderLog::mapped(log) });
    helios_kernel::complete_device_future_at_bring_up(helios_tpm::discover(
        interface, window, base, cpu, log,
    ))
    .unwrap_or_else(|error| {
        panic!("the TPM 2.0 at {base:#x} failed while measured boot was checked: {error}")
    })
}
