//! The machine's TPM 2.0, found through the ACPI TPM2 table and handed
//! to the kernel as its measured-boot device.
//!
//! Adapter code only: the table is found and its registers mapped here,
//! and everything else — reading the table, choosing the interface, the
//! checks that decide whether the kernel may use the TPM — is
//! `helios_tpm`'s.
//!
//! The TPM2 table names the interface by its start method. For the CRB
//! (method 7, QEMU's `tpm-crb` on `q35` and AWS NitroTPM) it names the
//! control area, whose page is locality 0. For the FIFO interface
//! (method 6) it names no address, because the PC Client Platform TPM
//! Profile fixes locality 0 at `0xfed40000` on every PC.
//!
//! Concurrency contract: discovery runs once, on the bootstrap
//! processor, before the kernel is initialised and before any secondary
//! starts. The device it yields is owned by the kernel's bring-up.

use core::ptr::NonNull;

use helios_hal::boot::MeasurementLog;
use helios_tpm::{
    AcpiTpm2, AcpiTpm2Locality, Discovered, Discovery, InterfaceDescription, InterfaceError,
    LOCALITY_BYTES, LoaderLog, MmioWindow, PC_CLIENT_FIFO_LOCALITY_0, Unqualified,
};

use crate::X86Cpu;

const TPM2_SIGNATURE: &str = "TPM2";

/// Finds the TPM the ACPI tables describe and runs the measured-boot
/// checks against it.
///
/// A TPM that fails a check is reported and not handed over; one that
/// fails while it is checked stops the boot.
pub(crate) fn discover(
    rsdp_address: usize,
    physical_memory_offset: usize,
    cpu: X86Cpu,
    measurement_log: Option<MeasurementLog>,
) -> Discovered<MmioWindow, X86Cpu> {
    let Some(table) = tpm2_table(rsdp_address, physical_memory_offset) else {
        return (None, Discovery::NotDescribed);
    };
    let table = AcpiTpm2::parse(table)
        .unwrap_or_else(|error| panic!("the ACPI TPM2 table is malformed: {error}"));
    let base = match table
        .locality()
        .unwrap_or_else(|error| panic!("the ACPI TPM2 table is malformed: {error}"))
    {
        AcpiTpm2Locality::Crb { base } => base,
        AcpiTpm2Locality::Fifo => PC_CLIENT_FIFO_LOCALITY_0,
        AcpiTpm2Locality::Unsupported { start_method } => {
            return (
                None,
                Discovery::Refused {
                    base: None,
                    reason: Unqualified::Interface(InterfaceError::StartMethod(start_method)),
                },
            );
        }
    };
    let base_address = usize::try_from(base)
        .unwrap_or_else(|_| panic!("the TPM at {base:#x} is outside the physical address space"));
    let virtual_base =
        crate::smp::map_mmio_window(physical_memory_offset, base_address, LOCALITY_BYTES);
    // SAFETY: `map_mmio_window` mapped locality 0's page as device
    // memory and never unmaps it, and nothing else in this kernel
    // touches the TPM's registers.
    let window = unsafe {
        MmioWindow::new(
            NonNull::new(virtual_base as *mut u8)
                .unwrap_or_else(|| panic!("the TPM's register page mapped to a null address")),
            LOCALITY_BYTES,
        )
    };
    // SAFETY: Limine's event log lives in bootloader-reclaimable memory,
    // which this backend never hands to the allocator (`boot.rs`), in
    // the direct map the response points into.
    let log = measurement_log.map(|log| unsafe { LoaderLog::mapped(&log) });
    helios_kernel::complete_device_future_at_bring_up(helios_tpm::discover(
        InterfaceDescription::AcpiStartMethod(table.start_method()),
        window,
        base,
        cpu,
        log,
    ))
    .unwrap_or_else(|error| {
        panic!("the TPM 2.0 at {base:#x} failed while measured boot was checked: {error}")
    })
}

/// The bytes of the TPM2 table, when the firmware published one.
fn tpm2_table(rsdp_address: usize, physical_memory_offset: usize) -> Option<&'static [u8]> {
    let handler = crate::smp::PhysicalOffsetAcpiHandler {
        physical_memory_offset,
        tsc_base: 0,
        timebase: helios_hal::cpu::Timebase::new(1),
    };
    // SAFETY: the RSDP is the one Limine read out of the EFI
    // configuration table, and `from_rsdp` validates it before following
    // it.
    let tables = unsafe { acpi::AcpiTables::from_rsdp(handler, rsdp_address) }
        .unwrap_or_else(|error| panic!("failed to parse ACPI tables for the TPM2: {error:?}"));
    let (physical_address, header) = tables
        .table_headers()
        .find(|(_, header)| header.signature.as_str() == TPM2_SIGNATURE)?;
    // `SdtHeader` is a packed structure, so the field is copied out
    // before anything takes a reference to it.
    let declared_length = header.length;
    let length = usize::try_from(declared_length)
        .unwrap_or_else(|_| panic!("ACPI TPM2 length {declared_length} does not fit usize"));
    let virtual_address = physical_address
        .checked_add(physical_memory_offset)
        .unwrap_or_else(|| panic!("ACPI TPM2 at {physical_address:#x} overflowed the HHDM"));
    // SAFETY: the address and length come from the RSDT entry the
    // firmware published, and firmware tables sit inside the direct
    // physical map the bootloader established.
    Some(unsafe { core::slice::from_raw_parts(virtual_address as *const u8, length) })
}
