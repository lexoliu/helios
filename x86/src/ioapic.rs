//! The I/O APICs that carry the machine's legacy interrupt lines.
//!
//! Every PCI device this backend drives signals through MSI-X, which a
//! local APIC receives with no routing hardware between it and the PCI
//! function. Two sources have no message-signalled path and raise an
//! ISA-numbered line only an I/O APIC can deliver: COM1 (IRQ 4) and the
//! ACPI SCI (the IRQ the FADT names). The redirection entry this driver
//! programs for each is what turns the line into a vector the interrupt
//! table dispatches.

use acpi::sdt::madt::{Madt, MadtEntry};
use acpi::{AcpiTables, Handler};
use alloc::vec::Vec;

use crate::smp::{LocalApicMode, map_mmio_window};

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

/// A legacy interrupt line, and how it signals when no MADT
/// interrupt-source override says otherwise.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LegacyLine {
    /// The ISA IRQ number; with no override it is also the global system
    /// interrupt.
    irq: u8,
    active_low: bool,
    level_triggered: bool,
}

/// COM1 asserts ISA IRQ 4, with the ISA bus's own signalling: edge,
/// active high.
pub(crate) const COM1: LegacyLine = LegacyLine {
    irq: 4,
    active_low: false,
    level_triggered: false,
};

impl LegacyLine {
    /// The SCI on the IRQ the FADT's `SCI_INT` names. ACPI makes it a
    /// shareable, level-triggered, active-low interrupt unless an
    /// interrupt-source override says otherwise (ACPI 6.5 §5.2.9, and
    /// §5.2.12.5 for the override); QEMU's q35 overrides IRQ 9 to level,
    /// active high (`hw/i386/acpi-common.c`).
    pub(crate) fn sci(sci_interrupt: u16) -> Self {
        let irq = u8::try_from(sci_interrupt)
            .ok()
            .filter(|irq| *irq < 16)
            .unwrap_or_else(|| {
                panic!(
                    "the FADT routes the SCI to interrupt {sci_interrupt}, which is not an ISA \
                     IRQ; this backend routes the SCI as a legacy line"
                )
            });
        Self {
            irq,
            active_low: true,
            level_triggered: true,
        }
    }
}

/// One I/O APIC: its register window and the global system interrupts
/// its redirection table implements.
struct Controller {
    /// IOREGSEL's higher-half virtual address; IOWIN sits 0x10 above it.
    registers: usize,
    gsi_base: u32,
    entries: u32,
}

/// An MADT interrupt-source override of an ISA IRQ.
#[derive(Clone, Copy)]
struct SourceOverride {
    irq: u8,
    gsi: u32,
    /// MPS INTI flags.
    flags: u16,
}

/// Every I/O APIC the MADT describes, and its ISA overrides.
///
/// The register windows stay mapped for the life of the machine. The
/// redirection entries are programmed on the bootstrap processor during
/// bring-up, after the interrupt routes are installed and before that
/// processor's interrupts are enabled, so no delivery can precede its
/// handler.
pub(crate) struct IoApics {
    controllers: Vec<Controller>,
    overrides: Vec<SourceOverride>,
    /// Decides the destination-field width a redirection entry takes.
    apic_mode: LocalApicMode,
}

/// Maps every I/O APIC the MADT lists and reads how many redirection
/// entries it implements, which the MADT does not say.
pub(crate) fn discover<H: Handler>(
    tables: &AcpiTables<H>,
    physical_memory_offset: usize,
    apic_mode: LocalApicMode,
) -> IoApics {
    let madt = tables
        .find_table::<Madt>()
        .unwrap_or_else(|| panic!("ACPI tables did not expose an MADT for I/O APIC discovery"));
    let mut controllers = Vec::new();
    let mut overrides = Vec::new();
    for entry in madt.get().entries() {
        match entry {
            MadtEntry::IoApic(io_apic) => {
                let address = io_apic.io_apic_address;
                let registers = map_mmio_window(
                    physical_memory_offset,
                    usize::try_from(address).unwrap_or_else(|_| {
                        panic!("MADT I/O APIC address {address:#x} does not fit usize")
                    }),
                    WINDOW_BYTES,
                );
                controllers.push(Controller {
                    registers,
                    gsi_base: io_apic.global_system_interrupt_base,
                    entries: ((window_read(registers, IOAPIC_VER) >> 16) & 0xff) + 1,
                });
            }
            MadtEntry::InterruptSourceOverride(overridden) if overridden.bus == 0 => {
                overrides.push(SourceOverride {
                    irq: overridden.irq,
                    gsi: overridden.global_system_interrupt,
                    flags: overridden.flags,
                });
            }
            _ => {}
        }
    }
    IoApics {
        controllers,
        overrides,
        apic_mode,
    }
}

impl IoApics {
    /// Programs `line`'s redirection entry: `vector` is delivered, fixed
    /// mode with a physical destination, to the local APIC
    /// `destination_apic_id` names.
    ///
    /// An interrupt-source override for the IRQ moves the line to a
    /// different GSI and replaces its default signalling; without one,
    /// the GSI is the IRQ number. Panics naming what was checked when no
    /// I/O APIC covers the GSI.
    pub(crate) fn program_redirection(
        &self,
        line: LegacyLine,
        vector: u8,
        destination_apic_id: u32,
    ) {
        let mut gsi = u32::from(line.irq);
        let mut active_low = line.active_low;
        let mut level_triggered = line.level_triggered;
        if let Some(overridden) = self.overrides.iter().find(|entry| entry.irq == line.irq) {
            gsi = overridden.gsi;
            // MPS INTI flags: bits 1:0 are polarity, bits 3:2 the trigger
            // mode, and "conforms to the bus" keeps the line's default.
            match overridden.flags & 0b11 {
                0b00 => {}
                0b01 => active_low = false,
                0b11 => active_low = true,
                reserved => panic!(
                    "MADT interrupt-source override for ISA IRQ {} carries reserved polarity {reserved:#b}",
                    line.irq
                ),
            }
            match overridden.flags & 0b1100 {
                0 => {}
                0b0100 => level_triggered = false,
                0b1100 => level_triggered = true,
                reserved => panic!(
                    "MADT interrupt-source override for ISA IRQ {} carries reserved trigger mode {reserved:#b}",
                    line.irq
                ),
            }
        }
        let (registers, pin) = self
            .controllers
            .iter()
            .find_map(|controller| {
                (gsi >= controller.gsi_base && gsi - controller.gsi_base < controller.entries)
                    .then_some((controller.registers, gsi - controller.gsi_base))
            })
            .unwrap_or_else(|| {
                panic!(
                    "no MADT I/O APIC entry covers GSI {gsi} (ISA IRQ {}): checked {} I/O APIC \
                     entries",
                    line.irq,
                    self.controllers.len()
                )
            });
        // Delivery mode fixed and destination mode physical are the
        // zero bits; only polarity, trigger and the mask carry values.
        let mut low = u32::from(vector);
        if active_low {
            low |= ENTRY_ACTIVE_LOW;
        }
        if level_triggered {
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
        let index = REDIRECTION_BASE + 2 * pin;
        // The entry stays masked while it is half-programmed: the low
        // dword is written masked, the destination lands, then the mask
        // lifts.
        window_write(registers, index, low | ENTRY_MASKED);
        window_write(registers, index + 1, high);
        window_write(registers, index, low);
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
