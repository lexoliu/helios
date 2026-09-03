//! ACPI, decoded into a [`PlatformDescription`].
//!
//! This is the description QEMU's `virt` board publishes with ACPI
//! enabled, and the only one an Arm SBBR server platform publishes at
//! all. Three sources feed the description:
//!
//! * the **MADT** gives the GIC distributor frame, the redistributor
//!   discovery range, and one GICC entry per processor naming its MPIDR
//!   and its own redistributor frame;
//! * the **SPCR** gives the console UART the firmware was already using
//!   for redirection, which is the same port the kernel takes over;
//! * the **DSDT** (plus any SSDTs) describes everything else as AML
//!   device objects, so the virtio-mmio transports are found by
//!   evaluating `_HID` and `_CRS`.
//!
//! The calendar is the one thing an ACPI-described machine does not
//! hand over here. A PL031 is an AMBA primecell with no ACPI hardware
//! ID — Linux's own driver binds it from the device tree only — and the
//! ACPI answer is the Time and Alarm Device, which this kernel does not
//! implement. So [`describe`] reports no RTC and the kernel's wall
//! clock reads as uptime on an ACPI-described machine, which is the
//! honest answer rather than a guessed `_HID` that would match nothing.
//!
//! Concurrency contract: everything here runs on the bootstrap
//! processor during bring-up, before any secondary is started. The AML
//! interpreter is built, used, and dropped inside [`describe`].

use acpi::address::AddressSpace;
use acpi::aml::namespace::{AmlName, NamespaceLevel};
use acpi::aml::object::Object;
use acpi::aml::resource::{self, MemoryRangeDescriptor, Resource};
use acpi::aml::{AmlError, Interpreter};
use acpi::registers::FixedRegisters;
use acpi::sdt::fadt::Fadt;
use acpi::sdt::madt::{Madt, MadtEntry};
use acpi::sdt::spcr::Spcr;
use acpi::sdt::{SdtHeader, Signature};
use acpi::{AcpiTables, Handle, Handler, PciAddress, PhysicalMapping};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use arm_gic::Trigger;
use core::str::FromStr;
use thiserror::Error;

use super::{
    ConsoleDescription, GicDescription, MmioRegion, PlatformDescription, PlatformSource,
    RedistributorAffinity, Slots, SpiInterrupt, VirtioMmioSlot,
};

/// The first 32 INTIDs are software-generated and private peripheral
/// interrupts; ACPI's global system interrupt vector for a shared
/// peripheral interrupt is `32 + SPI index`.
const FIRST_SPI_INTID: u32 = 32;

/// `_HID` of QEMU's virtio-mmio transport, an ARM Linaro identifier.
const VIRTIO_MMIO_HID: &str = "LNRO0005";

/// The `acpi` crate's own errors, plus the ones this decode adds.
#[derive(Debug, Error)]
pub(crate) enum AcpiError {
    #[error("the ACPI tables could not be read: {0:?}")]
    Tables(acpi::AcpiError),
    #[error("the ACPI tables contain no {0:?} table")]
    MissingTable(Signature),
    #[error("the SPCR declares no console: the firmware redirects to no serial port")]
    NoConsole,
    #[error(
        "the SPCR console is in address space {0:?}; AArch64 consoles are memory mapped and this \
         kernel has no other way to reach one"
    )]
    ConsoleAddressSpace(AddressSpace),
    #[error("the MADT declares no {0}")]
    MadtMissing(&'static str),
    #[error("interpreting the DSDT failed: {0:?}")]
    Aml(AmlError),
    #[error("the {0} device's _CRS declares no {1}")]
    ResourceMissing(String, &'static str),
    #[error(
        "global system interrupt {0} is below the first shared peripheral interrupt; only SPIs \
         can be routed to a processor"
    )]
    NotAnSpi(u32),
}

/// The ACPI tables Limine's RSDP led to.
pub(crate) struct AcpiPlatformTables {
    tables: AcpiTables<Aarch64AcpiHandler>,
    handler: Aarch64AcpiHandler,
}

/// Opens the tables at the given RSDP.
pub(super) fn open(rsdp: usize) -> Result<AcpiPlatformTables, AcpiError> {
    let handler = Aarch64AcpiHandler {
        physical_memory_offset: crate::physical_memory_offset(),
        timer_frequency: crate::timer_frequency(),
        bootstrap_mpidr: crate::read_mpidr_affinity(),
    };
    // SAFETY: the address is the one the bootloader read out of the EFI
    // configuration table, and `from_rsdp` validates the signature and
    // checksum before trusting anything it points at.
    let tables =
        unsafe { AcpiTables::from_rsdp(handler.clone(), rsdp) }.map_err(AcpiError::Tables)?;
    Ok(AcpiPlatformTables { tables, handler })
}

/// The console the firmware was redirecting to, from the SPCR.
///
/// Allocation-free, so this can run before the kernel has a heap: the
/// SPCR is a fixed-layout table read straight out of the mapping.
pub(super) fn console(tables: &AcpiPlatformTables) -> Result<ConsoleDescription, AcpiError> {
    let spcr = tables
        .tables
        .find_table::<Spcr>()
        .ok_or(AcpiError::MissingTable(Signature::SPCR))?;
    let address = spcr
        .base_address()
        .ok_or(AcpiError::NoConsole)?
        .map_err(AcpiError::Tables)?;
    if address.address_space != AddressSpace::SystemMemory {
        return Err(AcpiError::ConsoleAddressSpace(address.address_space));
    }
    Ok(ConsoleDescription {
        region: MmioRegion {
            base: address.address as usize,
            // The SPCR names the register block but not its length; a
            // PL011's registers fit in the one page every AArch64 MMIO
            // mapping in this backend is made of.
            size: crate::PAGE_BYTES,
        },
        interrupt: spcr
            .global_system_interrupt()
            .map(spi_from_gsiv)
            .transpose()?
            // A UART line is edge-triggered nowhere on this platform,
            // and the SPCR's interrupt-type flags say nothing about
            // trigger mode.
            .map(|number| SpiInterrupt {
                number,
                trigger: Trigger::Level,
            }),
    })
}

/// The GIC from the MADT and the devices from the DSDT.
///
/// Runs after the kernel heap exists. The AML interpreter allocates —
/// it builds the whole ACPI namespace as owned objects — and there is
/// no allocation-free way to reach a `_CRS`, because a `_CRS` may be a
/// method whose result is computed at run time. The allocation is from
/// the kernel pool, is bounded by the size of the firmware's own
/// tables, and is released when the interpreter is dropped at the end
/// of this function: nothing it builds outlives the description.
pub(super) fn describe(
    tables: &AcpiPlatformTables,
    console: ConsoleDescription,
) -> Result<PlatformDescription, AcpiError> {
    let gic = gic(tables)?;
    let interpreter = load_namespace(tables)?;
    let mut virtio = Slots::new();
    for (name, hid) in devices(&interpreter)? {
        if hid == VIRTIO_MMIO_HID {
            let (region, interrupt) = memory_and_interrupt_resource(&interpreter, &name)?;
            virtio.push(
                VirtioMmioSlot { region, interrupt },
                "virtio-mmio transports",
            );
        }
    }
    Ok(PlatformDescription {
        source: PlatformSource::Acpi,
        console,
        gic,
        // See the module comment: ACPI describes a calendar through the
        // Time and Alarm Device, not through the PL031 the device-tree
        // path finds.
        rtc: None,
        virtio,
        // ACPI has no counterpart to the device tree's
        // `/chosen/rng-seed`, so an ACPI-described machine starts from
        // the processor's own random source alone until the entropy
        // device joins it.
        boot_entropy_seed: None,
    })
}

fn gic(tables: &AcpiPlatformTables) -> Result<GicDescription, AcpiError> {
    let madt = tables
        .tables
        .find_table::<Madt>()
        .ok_or(AcpiError::MissingTable(Signature::MADT))?;
    let mut distributor = None;
    let mut redistributor = None;
    let mut redistributors = Slots::new();
    for entry in madt.get().entries() {
        match entry {
            MadtEntry::Gicd(gicd) => {
                distributor = Some(MmioRegion {
                    base: gicd.physical_base_address as usize,
                    // The MADT gives the distributor's base but not its
                    // length: GICD_* registers occupy a fixed 64KiB
                    // frame in every GIC architecture version.
                    size: GICD_FRAME_BYTES,
                });
            }
            MadtEntry::GicRedistributor(gicr) => {
                redistributor = Some(MmioRegion {
                    base: gicr.discovery_range_base_address as usize,
                    size: gicr.discovery_range_length as usize,
                });
            }
            MadtEntry::Gicc(gicc) => {
                redistributors.push(
                    RedistributorAffinity {
                        mpidr: gicc.mpidr,
                        base: gicc.gicr_base_address as usize,
                    },
                    "processors",
                );
            }
            _ => {}
        }
    }
    Ok(GicDescription {
        distributor: distributor.ok_or(AcpiError::MadtMissing("GIC distributor"))?,
        redistributor: redistributor
            .ok_or(AcpiError::MadtMissing("GIC redistributor discovery range"))?,
        redistributors,
    })
}

/// A GICD register frame is 64KiB in every GIC architecture version.
const GICD_FRAME_BYTES: usize = 0x1_0000;

/// Loads the DSDT and every SSDT into a fresh AML namespace.
fn load_namespace(
    tables: &AcpiPlatformTables,
) -> Result<Interpreter<Aarch64AcpiHandler>, AcpiError> {
    let fadt = tables
        .tables
        .find_table::<Fadt>()
        .ok_or(AcpiError::MissingTable(Signature::FADT))?;
    // On the hardware-reduced ACPI that every AArch64 platform uses,
    // the PM1 blocks are absent and map to an empty system-IO address
    // that the register block never touches. Constructing them is what
    // the interpreter's `Store` opcode needs to exist; nothing here
    // reads or writes them.
    // The `acpi` crate's interpreter takes its register block as an
    // `Arc`, and `FixedRegisters` is neither `Send` nor `Sync` because
    // it owns raw firmware mappings. That is exactly right here: the
    // interpreter, its registers and every object it builds live and
    // die on the bootstrap processor inside `describe`.
    #[expect(
        clippy::arc_with_non_send_sync,
        reason = "the interpreter's constructor takes an Arc and the whole interpreter is \
                  single-threaded, so the reference count is never shared across processors"
    )]
    let registers =
        Arc::new(FixedRegisters::new(&fadt, tables.handler.clone()).map_err(AcpiError::Tables)?);
    let dsdt = tables.tables.dsdt().map_err(AcpiError::Tables)?;
    let interpreter = Interpreter::new(tables.handler.clone(), dsdt.revision, registers, None);
    load_table(&interpreter, tables, dsdt)?;
    for ssdt in tables.tables.ssdts() {
        load_table(&interpreter, tables, ssdt)?;
    }
    Ok(interpreter)
}

fn load_table(
    interpreter: &Interpreter<Aarch64AcpiHandler>,
    tables: &AcpiPlatformTables,
    table: acpi::AmlTable,
) -> Result<(), AcpiError> {
    let header_bytes = core::mem::size_of::<SdtHeader>();
    // SAFETY: the address and length come from a table header the
    // `acpi` crate already validated, and the mapping is the direct
    // physical map the bootloader left in place.
    let stream = unsafe {
        let mapping = tables
            .handler
            .map_physical_region::<u8>(table.phys_address, table.length as usize);
        core::slice::from_raw_parts(
            mapping.virtual_start.as_ptr().add(header_bytes),
            table.length as usize - header_bytes,
        )
    };
    interpreter.load_table(stream).map_err(AcpiError::Aml)
}

/// Every device object in the namespace that names a hardware ID,
/// paired with that ID.
fn devices(
    interpreter: &Interpreter<Aarch64AcpiHandler>,
) -> Result<Vec<(AmlName, String)>, AcpiError> {
    let mut named = Vec::new();
    interpreter
        .namespace
        .lock()
        .traverse(|name, level: &NamespaceLevel| {
            if level.kind == acpi::aml::namespace::NamespaceLevelKind::Device {
                named.push(name.clone());
            }
            Ok(true)
        })
        .map_err(AcpiError::Aml)?;
    let mut devices = Vec::with_capacity(named.len());
    for name in named {
        let hid = child(&name, "_HID")?;
        let Some(object) = interpreter
            .evaluate_if_present(hid, Vec::new())
            .map_err(AcpiError::Aml)?
        else {
            continue;
        };
        // The two devices this backend looks for both spell their `_HID`
        // as a string. An integer `_HID` is a compressed EISA id, which
        // no QEMU or SBBR platform uses for these devices.
        if let Object::String(hid) = &*object {
            devices.push((name, hid.clone()));
        }
    }
    Ok(devices)
}

/// The single fixed memory range a device's `_CRS` declares.
fn memory_resource(
    interpreter: &Interpreter<Aarch64AcpiHandler>,
    device: &AmlName,
) -> Result<MmioRegion, AcpiError> {
    let resources = current_resources(interpreter, device)?;
    resources
        .iter()
        .find_map(|entry| match entry {
            Resource::MemoryRange(MemoryRangeDescriptor::FixedLocation {
                base_address,
                range_length,
                ..
            }) => Some(MmioRegion {
                base: *base_address as usize,
                size: *range_length as usize,
            }),
            _ => None,
        })
        .ok_or_else(|| AcpiError::ResourceMissing(alloc::format!("{device}"), "memory range"))
}

/// The fixed memory range and the interrupt a device's `_CRS` declares.
fn memory_and_interrupt_resource(
    interpreter: &Interpreter<Aarch64AcpiHandler>,
    device: &AmlName,
) -> Result<(MmioRegion, SpiInterrupt), AcpiError> {
    let region = memory_resource(interpreter, device)?;
    let resources = current_resources(interpreter, device)?;
    let irq = resources
        .iter()
        .find_map(|entry| match entry {
            Resource::Irq(irq) => Some(irq),
            _ => None,
        })
        .ok_or_else(|| AcpiError::ResourceMissing(alloc::format!("{device}"), "interrupt"))?;
    Ok((
        region,
        SpiInterrupt {
            number: spi_from_gsiv(irq.irq)?,
            trigger: match &irq.trigger {
                resource::InterruptTrigger::Edge => Trigger::Edge,
                resource::InterruptTrigger::Level => Trigger::Level,
            },
        },
    ))
}

fn current_resources(
    interpreter: &Interpreter<Aarch64AcpiHandler>,
    device: &AmlName,
) -> Result<Vec<Resource>, AcpiError> {
    let crs = interpreter
        .evaluate(child(device, "_CRS")?, Vec::new())
        .map_err(AcpiError::Aml)?;
    resource::resource_descriptor_list(crs).map_err(AcpiError::Aml)
}

fn child(device: &AmlName, segment: &str) -> Result<AmlName, AcpiError> {
    AmlName::from_str(segment)
        .and_then(|segment| segment.resolve(device))
        .map_err(AcpiError::Aml)
}

fn spi_from_gsiv(gsiv: u32) -> Result<u32, AcpiError> {
    gsiv.checked_sub(FIRST_SPI_INTID)
        .ok_or(AcpiError::NotAnSpi(gsiv))
}

/// Reaches physical memory through the bootloader's direct map.
///
/// Every address the ACPI tables name is either firmware memory, which
/// Limine leaves in the higher-half direct map, or a device register
/// window, which the caller maps before it is touched. Nothing here
/// creates a mapping, so `unmap_physical_region` has nothing to undo.
#[derive(Clone)]
pub(crate) struct Aarch64AcpiHandler {
    physical_memory_offset: usize,
    timer_frequency: u64,
    /// Affinity of the processor the interpreter is allowed to run on;
    /// see the mutex hooks below.
    bootstrap_mpidr: usize,
}

impl Aarch64AcpiHandler {
    fn physical_to_virtual<T>(&self, physical_address: usize) -> core::ptr::NonNull<T> {
        let virtual_address = physical_address
            .checked_add(self.physical_memory_offset)
            .unwrap_or_else(|| panic!("ACPI physical mapping overflow at {physical_address:#x}"));
        core::ptr::NonNull::new(virtual_address as *mut T)
            .unwrap_or_else(|| panic!("ACPI physical mapping produced a null virtual pointer"))
    }
}

impl Handler for Aarch64AcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        PhysicalMapping {
            physical_start: physical_address,
            virtual_start: self.physical_to_virtual(physical_address),
            region_length: size,
            mapped_length: size,
            handler: self.clone(),
        }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}

    fn read_u8(&self, address: usize) -> u8 {
        unsafe { (address as *const u8).read_volatile() }
    }

    fn read_u16(&self, address: usize) -> u16 {
        unsafe { (address as *const u16).read_volatile() }
    }

    fn read_u32(&self, address: usize) -> u32 {
        unsafe { (address as *const u32).read_volatile() }
    }

    fn read_u64(&self, address: usize) -> u64 {
        unsafe { (address as *const u64).read_volatile() }
    }

    fn write_u8(&self, address: usize, value: u8) {
        unsafe { (address as *mut u8).write_volatile(value) }
    }

    fn write_u16(&self, address: usize, value: u16) {
        unsafe { (address as *mut u16).write_volatile(value) }
    }

    fn write_u32(&self, address: usize, value: u32) {
        unsafe { (address as *mut u32).write_volatile(value) }
    }

    fn write_u64(&self, address: usize, value: u64) {
        unsafe { (address as *mut u64).write_volatile(value) }
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        panic!("AArch64 has no I/O port space; ACPI asked to read port {port:#x}")
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        panic!("AArch64 has no I/O port space; ACPI asked to read port {port:#x}")
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        panic!("AArch64 has no I/O port space; ACPI asked to read port {port:#x}")
    }

    fn write_io_u8(&self, port: u16, _value: u8) {
        panic!("AArch64 has no I/O port space; ACPI asked to write port {port:#x}")
    }

    fn write_io_u16(&self, port: u16, _value: u16) {
        panic!("AArch64 has no I/O port space; ACPI asked to write port {port:#x}")
    }

    fn write_io_u32(&self, port: u16, _value: u32) {
        panic!("AArch64 has no I/O port space; ACPI asked to write port {port:#x}")
    }

    fn read_pci_u8(&self, address: PciAddress, offset: u16) -> u8 {
        panic!("ACPI asked to read PCI config {address:?}+{offset:#x} before the host bridge is up")
    }

    fn read_pci_u16(&self, address: PciAddress, offset: u16) -> u16 {
        panic!("ACPI asked to read PCI config {address:?}+{offset:#x} before the host bridge is up")
    }

    fn read_pci_u32(&self, address: PciAddress, offset: u16) -> u32 {
        panic!("ACPI asked to read PCI config {address:?}+{offset:#x} before the host bridge is up")
    }

    fn write_pci_u8(&self, address: PciAddress, offset: u16, _value: u8) {
        panic!(
            "ACPI asked to write PCI config {address:?}+{offset:#x} before the host bridge is up"
        )
    }

    fn write_pci_u16(&self, address: PciAddress, offset: u16, _value: u16) {
        panic!(
            "ACPI asked to write PCI config {address:?}+{offset:#x} before the host bridge is up"
        )
    }

    fn write_pci_u32(&self, address: PciAddress, offset: u16, _value: u32) {
        panic!(
            "ACPI asked to write PCI config {address:?}+{offset:#x} before the host bridge is up"
        )
    }

    fn nanos_since_boot(&self) -> u64 {
        crate::read_counter()
            .saturating_mul(1_000_000_000)
            .wrapping_div(self.timer_frequency)
    }

    fn stall(&self, microseconds: u64) {
        let ticks = self
            .timer_frequency
            .saturating_mul(microseconds)
            .wrapping_div(1_000_000);
        let deadline = crate::read_counter().saturating_add(ticks);
        while crate::read_counter() < deadline {
            core::hint::spin_loop();
        }
    }

    fn sleep(&self, milliseconds: u64) {
        self.stall(milliseconds.saturating_mul(1_000));
    }

    /// AML mutexes need no state on this platform, and the handle is
    /// therefore the same for every one of them.
    ///
    /// A mutex exists to keep two threads of control out of the same
    /// AML region. The interpreter this backend builds has exactly one:
    /// it is created, used and dropped inside [`describe`], which runs
    /// on the bootstrap processor before any secondary is started and
    /// never yields to the executor, which does not exist yet. With one
    /// thread of control, every acquire succeeds immediately — including
    /// the recursive acquire AML is allowed to make — and every release
    /// has nothing to hand over. [`Self::acquire`] asserts the invariant
    /// rather than assuming it, so an interpreter that ever ran
    /// somewhere else would fail here instead of silently losing mutual
    /// exclusion.
    fn create_mutex(&self) -> Handle {
        Handle(0)
    }

    fn acquire(&self, _mutex: Handle, _timeout: u16) -> Result<(), AmlError> {
        assert!(
            crate::read_mpidr_affinity() == self.bootstrap_mpidr,
            "the AML interpreter ran outside the bootstrap processor, where its mutexes \
             would no longer provide mutual exclusion"
        );
        Ok(())
    }

    fn release(&self, _mutex: Handle) {}
}
