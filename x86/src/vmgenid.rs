//! The VM Generation ID window, found through ACPI.
//!
//! QEMU's `vmgenid` device is no PCI function and no MMIO block: the
//! machine's firmware tables carry an SSDT device object with
//! `_HID "QEMUVGID"` and `_CID "VM_Gen_Counter"`. Its `_STA` method says
//! whether firmware has written the window address, and its `ADDR` method
//! returns a two-integer package, the low and high halves of the
//! guest-physical address of a 16-byte window; QEMU rewrites that window
//! whenever the guest is restored from a snapshot or cloned
//! (`hw/acpi/vmgenid.c`). Both methods only read a dword the firmware
//! linker patched into the table, so no AML region access is involved.
//! The window sits in ordinary guest memory, so the bootloader's direct
//! map reaches it.
//!
//! This module finds the device by running the AML interpreter over the
//! DSDT and every SSDT, checks `_STA`, evaluates `ADDR`, and hands the
//! window to the machine generation. Which window it is and what a change in it means
//! are the kernel's and `hal`'s business; this is the discovery only.
//!
//! Concurrency contract: discovery runs once on the bootstrap processor
//! during bring-up, before any secondary starts and before the executor
//! exists. The interpreter is built, used and dropped here. The window is
//! never written by the guest.

use core::ptr::NonNull;
use core::str::FromStr;

use acpi::aml::namespace::{AmlName, NamespaceLevel, NamespaceLevelKind};
use acpi::aml::object::{DeviceStatus, Object};
use acpi::aml::{AmlError, Interpreter};
use acpi::registers::FixedRegisters;
use acpi::sdt::SdtHeader;
use acpi::sdt::fadt::Fadt;
use acpi::{AcpiTables, AmlTable};
use alloc::sync::Arc;
use alloc::vec::Vec;
use helios_hal::generation::{GenerationIdAddress, GenerationIdWindow};

use crate::smp::{PhysicalOffsetAcpiHandler, ensure_hhdm_range_mapped};

/// `_HID` of QEMU's VM Generation ID device (`hw/acpi/vmgenid.c`).
const VMGENID_HID: &str = "QEMUVGID";
/// `_CID` the VM Generation ID specification gives the device.
const VMGENID_CID: &str = "VM_Gen_Counter";
/// The window is a 128-bit identifier.
const WINDOW_BYTES: usize = 16;

/// A VM Generation ID device the firmware tables describe.
#[derive(Clone, Copy)]
pub(crate) struct VmGenerationIdDevice {
    /// Where `ADDR` says the window is, in guest-physical memory.
    pub(crate) address: GenerationIdAddress,
    /// The window, mapped through the direct map.
    pub(crate) window: GenerationIdWindow,
}

/// Looks for the VM Generation ID device and maps its window.
///
/// Returns `None` when the machine publishes no such device. A device
/// that is present but whose `ADDR` does not name a window is a firmware
/// bring-up fault and panics, as does a failure of the AML interpreter:
/// running on with no window would leave a restored guest unable to tell
/// it was restored.
pub(crate) fn discover(
    handler: PhysicalOffsetAcpiHandler,
    rsdp_address: usize,
) -> Option<VmGenerationIdDevice> {
    let physical_memory_offset = handler.physical_memory_offset;
    // SAFETY: the RSDP address comes from the bootloader's handoff and the
    // handler maps through the direct map it left in place.
    let tables = unsafe { AcpiTables::from_rsdp(handler.clone(), rsdp_address) }
        .unwrap_or_else(|error| panic!("failed to parse ACPI tables for vmgenid: {error:?}"));
    let interpreter = load_namespace(&tables, &handler);
    let device = find_device(&interpreter)?;
    // QEMU's `_STA` reports the device not present until firmware has
    // written the window address into the table (`VGIA == 0`). `ADDR`
    // alone cannot tell: it returns that dword plus the GUID offset, so
    // an unwritten address reads as a small non-zero number. An absent
    // `_STA` means present, per ACPI.
    let status = interpreter
        .evaluate_if_present(child(&device, "_STA"), Vec::new())
        .unwrap_or_else(|error| panic!("vmgenid: evaluating {device}._STA failed: {error:?}"));
    if let Some(status) = status {
        let status = status
            .as_integer()
            .unwrap_or_else(|error| panic!("vmgenid: {device}._STA is not an integer: {error:?}"));
        assert!(
            DeviceStatus(status).present(),
            "vmgenid: {device} exists but its _STA ({status:#x}) says it is not present: \
             firmware has not written the window address"
        );
    }
    let addr = interpreter
        .evaluate(child(&device, "ADDR"), Vec::new())
        .unwrap_or_else(|error| panic!("vmgenid: evaluating {device}.ADDR failed: {error:?}"));
    let Object::Package(elements) = &*addr else {
        panic!(
            "vmgenid: {device}.ADDR is a {:?}, not the two-integer package the VM Generation ID \
             specification defines",
            addr.typ()
        );
    };
    let words: Vec<u64> = elements
        .iter()
        .enumerate()
        .map(|(index, element)| {
            element.as_integer().unwrap_or_else(|error| {
                panic!("vmgenid: {device}.ADDR element {index} is not an integer: {error:?}")
            })
        })
        .collect();
    let address = GenerationIdAddress::from_acpi_addr(&words)
        .unwrap_or_else(|error| panic!("vmgenid: {device} is present but {error}"));
    let physical = usize::try_from(address.get())
        .unwrap_or_else(|_| panic!("vmgenid: window address {:#x} exceeds usize", address.get()));
    // The window is ordinary guest memory the VMM writes. The direct map
    // normally covers it; where the bootloader left it out it is mapped
    // write-back here, as for any ACPI table.
    ensure_hhdm_range_mapped(physical_memory_offset, physical, WINDOW_BYTES);
    let virtual_address = physical_memory_offset
        .checked_add(physical)
        .unwrap_or_else(|| panic!("vmgenid: window address {physical:#x} overflows the HHDM"));
    let pointer = NonNull::new(virtual_address as *mut u64)
        .unwrap_or_else(|| panic!("vmgenid: window mapped to a null pointer"));
    // SAFETY: `from_acpi_addr` checked the address is non-zero and aligned
    // to 8, `ensure_hhdm_range_mapped` mapped the 16 bytes, and the
    // mapping is never removed.
    let window = unsafe { GenerationIdWindow::new(pointer) };
    Some(VmGenerationIdDevice { address, window })
}

/// Loads the DSDT and every SSDT into a fresh AML namespace.
fn load_namespace(
    tables: &AcpiTables<PhysicalOffsetAcpiHandler>,
    handler: &PhysicalOffsetAcpiHandler,
) -> Interpreter<PhysicalOffsetAcpiHandler> {
    let fadt = tables
        .find_table::<Fadt>()
        .unwrap_or_else(|| panic!("vmgenid: the ACPI tables carry no FADT"));
    // The interpreter's constructor takes its register block as an `Arc`,
    // and `FixedRegisters` owns raw firmware mappings, so it is neither
    // `Send` nor `Sync`. The interpreter and its registers live and die on
    // the bootstrap processor inside `discover`.
    #[expect(
        clippy::arc_with_non_send_sync,
        reason = "the interpreter's constructor takes an Arc and the whole interpreter is \
                  single-threaded, so the reference count is never shared across processors"
    )]
    let registers = Arc::new(
        FixedRegisters::new(&fadt, handler.clone())
            .unwrap_or_else(|error| panic!("vmgenid: FADT fixed registers: {error:?}")),
    );
    let dsdt = tables
        .dsdt()
        .unwrap_or_else(|error| panic!("vmgenid: the ACPI tables carry no DSDT: {error:?}"));
    let interpreter = Interpreter::new(handler.clone(), dsdt.revision, registers, None);
    load_table(&interpreter, handler, dsdt);
    for ssdt in tables.ssdts() {
        load_table(&interpreter, handler, ssdt);
    }
    interpreter
}

fn load_table(
    interpreter: &Interpreter<PhysicalOffsetAcpiHandler>,
    handler: &PhysicalOffsetAcpiHandler,
    table: AmlTable,
) {
    use acpi::Handler;

    let header_bytes = core::mem::size_of::<SdtHeader>();
    // SAFETY: the address and length come from a table header the `acpi`
    // crate already validated, and the mapping is the direct physical map
    // the bootloader left in place.
    let stream = unsafe {
        let mapping = handler.map_physical_region::<u8>(table.phys_address, table.length as usize);
        core::slice::from_raw_parts(
            mapping.virtual_start.as_ptr().add(header_bytes),
            table.length as usize - header_bytes,
        )
    };
    interpreter
        .load_table(stream)
        .unwrap_or_else(|error| panic!("vmgenid: loading an AML table failed: {error:?}"));
}

/// The device whose `_HID` or `_CID` names the VM Generation ID.
fn find_device(interpreter: &Interpreter<PhysicalOffsetAcpiHandler>) -> Option<AmlName> {
    let mut devices = Vec::new();
    interpreter
        .namespace
        .lock()
        .traverse(|name, level: &NamespaceLevel| {
            if level.kind == NamespaceLevelKind::Device {
                devices.push(name.clone());
            }
            Ok(true)
        })
        .unwrap_or_else(|error| panic!("vmgenid: walking the AML namespace failed: {error:?}"));
    devices.into_iter().find(|device| {
        names_id(interpreter, device, "_HID", VMGENID_HID)
            || names_id(interpreter, device, "_CID", VMGENID_CID)
    })
}

/// Whether `device.segment` is the string `expected`. An integer id is a
/// compressed EISA id, which this device never uses, so it never matches.
fn names_id(
    interpreter: &Interpreter<PhysicalOffsetAcpiHandler>,
    device: &AmlName,
    segment: &str,
    expected: &str,
) -> bool {
    let object = interpreter
        .evaluate_if_present(child(device, segment), Vec::new())
        .unwrap_or_else(|error| panic!("vmgenid: evaluating {device}.{segment}: {error:?}"));
    matches!(object.as_deref(), Some(Object::String(id)) if id == expected)
}

fn child(device: &AmlName, segment: &str) -> AmlName {
    AmlName::from_str(segment)
        .and_then(|segment| segment.resolve(device))
        .unwrap_or_else(|error: AmlError| panic!("vmgenid: naming {device}.{segment}: {error:?}"))
}
