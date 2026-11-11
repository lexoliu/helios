//! The I/O APIC that carries COM1's interrupt.
//!
//! Every other device this backend drives signals through MSI-X, which a
//! local APIC receives with no routing hardware between it and the PCI
//! function. A 16550 has no message-signalled path: COM1 raises ISA IRQ
//! 4 on a line only an I/O APIC can deliver, so the redirection entry
//! this driver programs is what turns a byte arriving on the debug
//! console into a vector the interrupt table dispatches.

use acpi::sdt::madt::{Madt, MadtEntry};
use acpi::{AcpiTables, Handler};

use crate::smp::{LocalApicMode, map_mmio_window};

/// COM1 asserts ISA IRQ 4. With no MADT interrupt-source override the
/// ISA identity applies and the line is the same-numbered global system
/// interrupt.
const COM1_ISA_IRQ: u8 = 4;

/// The register window is indirect: IOREGSEL selects the internal
/// register and IOWIN reads or writes it.
const IOWIN_OFFSET: usize = 0x10;
const WINDOW_BYTES: usize = 0x20;
/// The I/O APIC register indexes this driver touches.
const IOAPIC_VER: u32 = 0x01;
const REDIRECTION_BASE: u32 = 0x10;

const ENTRY_ACTIVE_LOW: u32 = 1 << 13;
const ENTRY_LEVEL_TRIGGERED: u32 = 1 << 15;
const ENTRY_MASKED: u32 = 1 << 16;

/// An I/O APIC whose COM1 redirection pin is already resolved.
///
/// The register window stays mapped for the life of the machine. The
/// one mutation the driver performs — programming the redirection entry
/// — runs once on the bootstrap processor, after the interrupt routes
/// are installed and before that processor's interrupts are enabled, so
/// no delivery can precede its handler.
pub(crate) struct IoApic {
    /// IOREGSEL's higher-half virtual address; IOWIN sits 0x10 above it.
    registers: usize,
    /// The redirection-table index COM1's GSI lands on.
    pin: u32,
    /// Polarity and trigger, from the MADT interrupt-source override
    /// when one covers ISA IRQ 4 and the ISA defaults otherwise.
    active_low: bool,
    level_triggered: bool,
    /// Decides the destination-field width the redirection entry takes.
    apic_mode: LocalApicMode,
}

/// Finds the I/O APIC that covers COM1's global system interrupt and
/// resolves the redirection pin, polarity and trigger mode for it.
///
/// An interrupt-source override for ISA IRQ 4 moves the line to a
/// different GSI and replaces the ISA defaults (edge-triggered, active
/// high); without one, the GSI is the IRQ number. Panics naming what
/// was checked when the MADT has no I/O APIC covering the GSI.
pub(crate) fn discover<H: Handler>(
    tables: &AcpiTables<H>,
    physical_memory_offset: usize,
    apic_mode: LocalApicMode,
) -> IoApic {
    let madt = tables
        .find_table::<Madt>()
        .unwrap_or_else(|| panic!("ACPI tables did not expose an MADT for I/O APIC discovery"));
    let mut gsi = u32::from(COM1_ISA_IRQ);
    let mut active_low = false;
    let mut level_triggered = false;
    for entry in madt.get().entries() {
        let MadtEntry::InterruptSourceOverride(overridden) = entry else {
            continue;
        };
        if overridden.bus != 0 || overridden.irq != COM1_ISA_IRQ {
            continue;
        }
        gsi = overridden.global_system_interrupt;
        // MPS INTI flags: bits 1:0 are polarity, bits 3:2 the trigger
        // mode, and "conforms to the bus" means the ISA defaults.
        match overridden.flags & 0b11 {
            0b00 | 0b01 => {}
            0b11 => active_low = true,
            reserved => panic!(
                "MADT interrupt-source override for ISA IRQ {COM1_ISA_IRQ} carries reserved polarity {reserved:#b}"
            ),
        }
        match overridden.flags & 0b1100 {
            0 | 0b0100 => {}
            0b1100 => level_triggered = true,
            reserved => panic!(
                "MADT interrupt-source override for ISA IRQ {COM1_ISA_IRQ} carries reserved trigger mode {reserved:#b}"
            ),
        }
    }
    // A controller covers a GSI that sits in the range its redirection
    // table implements, [gsi_base, gsi_base + entries): the count is not
    // in the MADT, so each candidate's window is mapped and its version
    // register read.
    let mut checked = 0_usize;
    let covering = madt.get().entries().find_map(|entry| {
        let MadtEntry::IoApic(io_apic) = entry else {
            return None;
        };
        checked += 1;
        let address = io_apic.io_apic_address;
        let registers = map_mmio_window(
            physical_memory_offset,
            usize::try_from(address).unwrap_or_else(|_| {
                panic!("MADT I/O APIC address {address:#x} does not fit usize")
            }),
            WINDOW_BYTES,
        );
        let entries = ((window_read(registers, IOAPIC_VER) >> 16) & 0xff) + 1;
        let gsi_base = io_apic.global_system_interrupt_base;
        (gsi >= gsi_base && gsi - gsi_base < entries).then_some((registers, gsi - gsi_base))
    });
    let (registers, pin) = covering.unwrap_or_else(|| {
        panic!(
            "no MADT I/O APIC entry covers GSI {gsi} (COM1, ISA IRQ {COM1_ISA_IRQ}): \
             checked {checked} I/O APIC entries"
        )
    });
    IoApic {
        registers,
        pin,
        active_low,
        level_triggered,
        apic_mode,
    }
}

impl IoApic {
    /// Programs COM1's redirection entry: `vector` is delivered, fixed
    /// mode with a physical destination, to the local APIC
    /// `destination_apic_id` names.
    pub(crate) fn program_redirection(&self, vector: u8, destination_apic_id: u32) {
        // Delivery mode fixed and destination mode physical are the
        // zero bits; only polarity, trigger and the mask carry values.
        let mut low = u32::from(vector);
        if self.active_low {
            low |= ENTRY_ACTIVE_LOW;
        }
        if self.level_triggered {
            low |= ENTRY_LEVEL_TRIGGERED;
        }
        let high = match self.apic_mode {
            // The xAPIC destination field is the top eight bits of the
            // entry's high dword; under x2APIC it widens to all 32 bits.
            LocalApicMode::XApic { .. } => {
                u32::from(LocalApicMode::xapic_target(destination_apic_id)) << 24
            }
            LocalApicMode::X2Apic => destination_apic_id,
        };
        let index = REDIRECTION_BASE + 2 * self.pin;
        // The entry stays masked while it is half-programmed: the low
        // dword is written masked, the destination lands, then the mask
        // lifts.
        window_write(self.registers, index, low | ENTRY_MASKED);
        window_write(self.registers, index + 1, high);
        window_write(self.registers, index, low);
    }
}

fn window_read(registers: usize, index: u32) -> u32 {
    unsafe {
        (registers as *mut u32).write_volatile(index);
        ((registers + IOWIN_OFFSET) as *const u32).read_volatile()
    }
}

fn window_write(registers: usize, index: u32, value: u32) {
    unsafe {
        (registers as *mut u32).write_volatile(index);
        ((registers + IOWIN_OFFSET) as *mut u32).write_volatile(value);
    }
}
