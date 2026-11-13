//! x86 PCI adaptation: legacy (0xCF8/0xCFC) configuration access, bus
//! enumeration, BAR mapping and MSI-X vector binding.
//!
//! q35 exposes every device Helios cares about through segment 0, so the
//! legacy configuration mechanism is sufficient and avoids depending on
//! the ACPI MCFG table for device discovery. Extended (>0xff) config
//! space is only needed by the watchdog, which keeps its own ECAM
//! accessor.

use alloc::vec::Vec;
use core::ptr::NonNull;

use helios_hal::io::{IoError, IoResult};
use helios_virtio::{DeviceType, PciMmioMapper};
use pci_types::capability::PciCapability;
use pci_types::{Bar, CommandRegister, ConfigRegionAccess, EndpointHeader, PciAddress, PciHeader};
use x86_64::instructions::port::{PortReadOnly, PortWriteOnly};

const PCI_CONFIG_ADDRESS_PORT: u16 = 0x0cf8;
const PCI_CONFIG_DATA_PORT: u16 = 0x0cfc;
const PCI_INVALID_VENDOR_ID: u16 = 0xffff;
const PCI_MAX_DEVICES: u8 = 32;
const PCI_MAX_FUNCTIONS: u8 = 8;

/// Local-APIC message address range used by MSI/MSI-X on x86.
const MSI_ADDRESS_BASE: u64 = 0xfee0_0000;
const MSI_DESTINATION_SHIFT: u32 = 12;

/// MSI-X table entry layout, in bytes.
const MSIX_ENTRY_BYTES: usize = 16;
const MSIX_ENTRY_ADDRESS_LOW: usize = 0x0;
const MSIX_ENTRY_ADDRESS_HIGH: usize = 0x4;
const MSIX_ENTRY_DATA: usize = 0x8;
const MSIX_ENTRY_VECTOR_CONTROL: usize = 0xc;
const MSIX_VECTOR_CONTROL_MASKED: u32 = 1 << 0;

/// The MSI-X table entry Helios binds a single-message function to.
/// One entry per device keeps the interrupt path a single IDT vector,
/// which is all a device whose queues are drained by one processor
/// needs.
pub(crate) const MSIX_VIRTIO_ENTRY: u16 = 0;

/// One MSI-X message: the IDT vector it raises and the local APIC it is
/// delivered to.
///
/// The destination is what makes per-queue vectors worth having — a
/// queue pair's completions are delivered to the processor that drains
/// that pair, instead of to whichever processor owns the function's only
/// vector and then has to hand the work across a core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MsixMessage {
    pub(crate) vector: u8,
    pub(crate) destination_apic_id: u32,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct LegacyPciConfigAccess;

impl LegacyPciConfigAccess {
    pub(crate) const fn new() -> Self {
        Self
    }

    pub(crate) fn read_u8(&self, address: PciAddress, offset: u16) -> u8 {
        let aligned_offset = offset & !0b11;
        let shift = ((offset & 0b11) * 8) as u32;
        ((unsafe { self.read(address, aligned_offset) } >> shift) & 0xff) as u8
    }

    pub(crate) fn read_u16(&self, address: PciAddress, offset: u16) -> u16 {
        let aligned_offset = offset & !0b11;
        let shift = ((offset & 0b10) * 8) as u32;
        ((unsafe { self.read(address, aligned_offset) } >> shift) & 0xffff) as u16
    }

    pub(crate) fn write_u8(&self, address: PciAddress, offset: u16, value: u8) {
        let aligned_offset = offset & !0b11;
        let shift = ((offset & 0b11) * 8) as u32;
        let mut data = unsafe { self.read(address, aligned_offset) };
        data &= !(0xff_u32 << shift);
        data |= u32::from(value) << shift;
        unsafe { self.write(address, aligned_offset, data) };
    }

    pub(crate) fn write_u16(&self, address: PciAddress, offset: u16, value: u16) {
        let aligned_offset = offset & !0b11;
        let shift = ((offset & 0b10) * 8) as u32;
        let mut data = unsafe { self.read(address, aligned_offset) };
        data &= !(0xffff_u32 << shift);
        data |= u32::from(value) << shift;
        unsafe { self.write(address, aligned_offset, data) };
    }

    fn config_address(address: PciAddress, offset: u16) -> u32 {
        assert!(
            address.segment() == 0,
            "legacy PCI config-space access only supports segment 0, got {}",
            address.segment()
        );
        assert!(
            offset & 0b11 == 0,
            "legacy PCI config-space access requires aligned offsets, got {offset:#x}"
        );
        assert!(
            offset < 0x100,
            "legacy PCI config-space access only supports the first 256 bytes, got {offset:#x}"
        );

        (1_u32 << 31)
            | (u32::from(address.bus()) << 16)
            | (u32::from(address.device()) << 11)
            | (u32::from(address.function()) << 8)
            | u32::from(offset)
    }
}

impl ConfigRegionAccess for LegacyPciConfigAccess {
    unsafe fn read(&self, address: PciAddress, offset: u16) -> u32 {
        critical_section::with(|_| unsafe {
            PortWriteOnly::<u32>::new(PCI_CONFIG_ADDRESS_PORT)
                .write(Self::config_address(address, offset));
            PortReadOnly::<u32>::new(PCI_CONFIG_DATA_PORT).read()
        })
    }

    unsafe fn write(&self, address: PciAddress, offset: u16, value: u32) {
        critical_section::with(|_| unsafe {
            PortWriteOnly::<u32>::new(PCI_CONFIG_ADDRESS_PORT)
                .write(Self::config_address(address, offset));
            PortWriteOnly::<u32>::new(PCI_CONFIG_DATA_PORT).write(value);
        });
    }
}

/// The x86 PCI root: configuration access plus the HHDM offset every BAR
/// mapping is expressed against.
#[derive(Clone, Copy)]
pub(crate) struct PciRoot {
    access: LegacyPciConfigAccess,
    physical_memory_offset: usize,
}

impl PciRoot {
    pub(crate) const fn new(physical_memory_offset: usize) -> Self {
        Self {
            access: LegacyPciConfigAccess::new(),
            physical_memory_offset,
        }
    }

    pub(crate) fn access(&self) -> LegacyPciConfigAccess {
        self.access
    }

    /// Every function present in segment 0.
    ///
    /// Buses are walked exhaustively rather than followed through
    /// bridge secondary-bus registers: with a single configuration
    /// segment the bus number alone addresses a function, and the walk
    /// then covers devices behind bridges without the kernel having to
    /// re-derive the firmware's bridge numbering.
    ///
    /// A slot is probed at function 0 alone unless that function is
    /// there and sets the multi-function bit of its header type: the PCI
    /// Local Bus Specification requires every device to implement
    /// function 0 and to set that bit when it implements any other, so
    /// an empty slot costs one configuration read instead of eight.
    /// Under a hypervisor every one of those reads is a pair of port
    /// accesses that exit to the device model.
    fn functions(&self) -> impl Iterator<Item = PciAddress> + '_ {
        (0..=u8::MAX).flat_map(move |bus| {
            (0..PCI_MAX_DEVICES).flat_map(move |device| {
                let first = PciAddress::new(0, bus, device, 0);
                let header = PciHeader::new(first);
                let (vendor_id, _) = header.id(self.access);
                let functions = if vendor_id == PCI_INVALID_VENDOR_ID {
                    0
                } else if header.has_multiple_functions(self.access) {
                    PCI_MAX_FUNCTIONS
                } else {
                    1
                };
                (0..functions).filter_map(move |function| {
                    if function == 0 {
                        return Some(first);
                    }
                    let address = PciAddress::new(0, bus, device, function);
                    let (vendor_id, _) = PciHeader::new(address).id(self.access);
                    (vendor_id != PCI_INVALID_VENDOR_ID).then_some(address)
                })
            })
        })
    }

    /// Every virtio function in segment 0, found by one walk of the bus.
    ///
    /// Each device class is looked up in what this returns rather than by
    /// a walk of its own, so boot pays for the bus once however many
    /// classes the backend drives.
    pub(crate) fn virtio_functions(&self) -> VirtioFunctions {
        let functions = self
            .functions()
            .filter_map(|address| {
                helios_virtio::virtio_pci_device_type(&self.access, address)
                    .map(|device_type| (address, device_type))
            })
            .collect();
        VirtioFunctions {
            root: *self,
            functions,
        }
    }

    /// Points one MSI-X table entry of `address` at `vector` on the
    /// local APIC of `destination_apic_id` and enables MSI-X on the
    /// function.
    ///
    /// Returns the table index the virtio common configuration should
    /// bind its queues to.
    pub(crate) fn bind_msix_vector(
        &self,
        address: PciAddress,
        vector: u8,
        destination_apic_id: u32,
    ) -> helios_virtio::MsixBinding {
        let table = self.bind_msix_vectors(
            address,
            &[MsixMessage {
                vector,
                destination_apic_id,
            }],
        );
        helios_virtio::MsixBinding::shared(table.first_entry())
    }

    /// Entries in the MSI-X table of `address`, or `None` when the
    /// function exposes no MSI-X capability at all.
    ///
    /// The table is the device's, and a function that offers four
    /// entries steers four messages however many processors the machine
    /// has. Callers that size a request from something on this side of
    /// the bus — a processor count, a queue-pair count — bound it by
    /// this first; [`Self::bind_msix_vectors`] treats overrunning the
    /// table as the programming error it is rather than truncating
    /// silently.
    pub(crate) fn msix_table_size(&self, address: PciAddress) -> Option<u16> {
        let header = PciHeader::new(address);
        let endpoint = EndpointHeader::from_header(header, self.access)?;
        endpoint
            .capabilities(self.access)
            .find_map(|capability| match capability {
                PciCapability::MsiX(msix) => Some(msix.table_size()),
                _ => None,
            })
    }

    /// Points the first `messages.len()` MSI-X table entries of
    /// `address` at the given vectors and local APICs, and enables MSI-X
    /// on the function.
    ///
    /// Returns the programmed table, whose first entry is always zero;
    /// the caller decides which structure each entry belongs to, and may
    /// later point one at another processor ([`MsixTable::retarget`]).
    ///
    /// # Concurrency contract
    ///
    /// Called during single-processor bring-up, before the vectors it
    /// programs can fire. The destinations may name secondary
    /// processors that are not started yet: a message to a parked local
    /// APIC is held by it, not lost.
    pub(crate) fn bind_msix_vectors(
        &self,
        address: PciAddress,
        messages: &[MsixMessage],
    ) -> MsixTable {
        let header = PciHeader::new(address);
        let mut endpoint = EndpointHeader::from_header(header, self.access)
            .unwrap_or_else(|| panic!("PCI function {address} is not an endpoint"));
        let mut capability = endpoint
            .capabilities(self.access)
            .find_map(|capability| match capability {
                PciCapability::MsiX(msix) => Some(msix),
                _ => None,
            })
            .unwrap_or_else(|| panic!("PCI function {address} exposes no MSI-X capability"));
        assert!(
            usize::from(capability.table_size()) >= messages.len(),
            "PCI function {address} has an MSI-X table of {} entries, too small for {} messages",
            capability.table_size(),
            messages.len()
        );

        // Sizing a BAR drives all-ones probe values through it, so
        // memory decode has to be off for the duration.
        endpoint.update_command(self.access, |command| {
            command & !CommandRegister::MEMORY_ENABLE
        });
        let table_bar = read_memory_bar(&endpoint, self.access, capability.table_bar());
        endpoint.update_command(self.access, |command| {
            command | CommandRegister::MEMORY_ENABLE
        });

        let table_offset = usize::try_from(capability.table_offset())
            .unwrap_or_else(|_| panic!("MSI-X table offset for {address} does not fit in usize"));
        let base = self
            .map_region(
                table_bar + table_offset as u64,
                MSIX_ENTRY_BYTES * messages.len(),
            )
            .unwrap_or_else(|error| panic!("failed to map the MSI-X table of {address}: {error}"))
            .cast::<u32>();
        let table = MsixTable {
            base,
            entries: u16::try_from(messages.len()).unwrap_or_else(|_| {
                panic!(
                    "{} MSI-X messages exceed the table's u16 index",
                    messages.len()
                )
            }),
        };

        for (index, message) in messages.iter().enumerate() {
            table.write_entry(index as u16, *message);
        }

        capability.set_function_mask(false, self.access);
        capability.set_enabled(true, self.access);
        assert!(
            capability.enabled(self.access),
            "PCI function {address} refused to enable MSI-X"
        );
        table
    }
}

/// The MSI-X table entries [`PciRoot::bind_msix_vectors`] programmed,
/// mapped for the kernel's lifetime.
///
/// # Concurrency contract
///
/// Entries are written at bring-up and afterwards only by
/// [`Self::retarget`], whose caller serialises it per entry. Every write
/// masks the entry first and unmasks it last, so the function never
/// sends a message from a half-written entry; a message it raises while
/// the entry is masked is held in the entry's pending bit, as MSI-X
/// per-vector masking requires, and sent to the new destination on the
/// unmask, so nothing is lost.
#[derive(Clone, Copy)]
pub(crate) struct MsixTable {
    base: NonNull<u32>,
    entries: u16,
}

// SAFETY: `base` is a device register window mapped for the kernel's
// lifetime, and every access to it is a volatile write of one aligned
// dword to an entry the caller serialises (see the type's contract).
unsafe impl Send for MsixTable {}
// SAFETY: as for `Send`: shared handles only ever issue the serialised
// volatile writes `retarget` makes.
unsafe impl Sync for MsixTable {}

impl MsixTable {
    /// The first entry programmed.
    pub(crate) const fn first_entry(self) -> u16 {
        MSIX_VIRTIO_ENTRY
    }

    /// Points `entry` at `message` from the next message on, while the
    /// function runs.
    pub(crate) fn retarget(self, entry: u16, message: MsixMessage) {
        assert!(
            message.destination_apic_id <= u32::from(u8::MAX),
            "MSI-X without interrupt remapping cannot target APIC id {}",
            message.destination_apic_id
        );
        self.write_entry(entry, message);
    }

    fn write_entry(self, index: u16, message: MsixMessage) {
        assert!(
            index < self.entries,
            "MSI-X entry {index} is outside the {} entries this table programmed",
            self.entries
        );
        // SAFETY: `index` is inside the window mapped for `entries`
        // entries of `MSIX_ENTRY_BYTES` each.
        let entry = unsafe {
            self.base
                .as_ptr()
                .byte_add(usize::from(index) * MSIX_ENTRY_BYTES)
        };
        let message_address =
            MSI_ADDRESS_BASE | (u64::from(message.destination_apic_id) << MSI_DESTINATION_SHIFT);
        // SAFETY: each offset is an aligned dword register of this
        // entry, inside the mapped window.
        unsafe {
            entry
                .byte_add(MSIX_ENTRY_VECTOR_CONTROL)
                .write_volatile(MSIX_VECTOR_CONTROL_MASKED);
            entry
                .byte_add(MSIX_ENTRY_ADDRESS_LOW)
                .write_volatile(message_address as u32);
            entry
                .byte_add(MSIX_ENTRY_ADDRESS_HIGH)
                .write_volatile((message_address >> 32) as u32);
            entry
                .byte_add(MSIX_ENTRY_DATA)
                .write_volatile(u32::from(message.vector));
            entry.byte_add(MSIX_ENTRY_VECTOR_CONTROL).write_volatile(0);
        }
    }
}

/// The virtio functions of segment 0 and the device class each carries,
/// in bus order, as one walk of the bus found them.
pub(crate) struct VirtioFunctions {
    root: PciRoot,
    functions: Vec<(PciAddress, DeviceType)>,
}

impl VirtioFunctions {
    /// Locates the first modern virtio function of the requested type.
    pub(crate) fn find_virtio_function(&self, device_type: DeviceType) -> Option<PciAddress> {
        self.find_virtio_functions(device_type).next()
    }

    /// Every modern virtio function of the requested type, in bus order.
    ///
    /// Device classes the platform exposes more than once — block
    /// devices, where the boot image and the kernel's own disk sit on
    /// the same bus — are brought up from this rather than from the
    /// first match.
    pub(crate) fn find_virtio_functions(
        &self,
        device_type: DeviceType,
    ) -> impl Iterator<Item = PciAddress> + '_ {
        self.functions
            .iter()
            .filter(move |(_, function_type)| *function_type == device_type)
            .map(|(address, _)| *address)
            .filter(move |address| {
                // MSI-X is the only interrupt path this backend programs:
                // every driver it installs is bound to a message, and there
                // is no INTx fallback to fall back to. A function without
                // one is a function the kernel cannot drive, which is the
                // same situation as the device not being there — and the
                // callers all handle that. Panicking the machine over an
                // optional device would not.
                if self.root.msix_table_size(*address).is_none() {
                    tracing::warn!(
                        function = %address,
                        ?device_type,
                        "ignoring a virtio function that exposes no MSI-X capability"
                    );
                    return false;
                }
                true
            })
    }
}

impl PciMmioMapper for PciRoot {
    fn map_region(&self, bar_address: u64, bytes: usize) -> IoResult<NonNull<u8>> {
        let physical_start = usize::try_from(bar_address).map_err(|_| IoError::OutOfBounds)?;
        let virtual_start =
            crate::smp::map_mmio_window(self.physical_memory_offset, physical_start, bytes);
        NonNull::new(virtual_start as *mut u8).ok_or(IoError::DeviceFault)
    }
}

fn read_memory_bar(endpoint: &EndpointHeader, access: LegacyPciConfigAccess, slot: u8) -> u64 {
    match endpoint.bar(slot, access) {
        Some(Bar::Memory32 { address, .. }) => u64::from(address),
        Some(Bar::Memory64 { address, .. }) => address,
        Some(Bar::Io { .. }) | None => {
            panic!("PCI BAR{slot} is not an assigned memory BAR")
        }
    }
}
