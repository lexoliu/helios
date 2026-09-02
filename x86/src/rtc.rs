//! mc146818 CMOS real-time clock for the x86 backend.
//!
//! The PC's calendar lives in the CMOS bank behind the index/data port
//! pair at 0x70/0x71. Each field is its own register, the bank declares
//! in status register B whether it reports binary-coded decimal and
//! whether hours run to 12 or 24, and the century — which the register
//! set has no room for — is a register the ACPI FADT points at.
//!
//! The bank updates itself once a second and marks that window in
//! status register A. A read therefore waits for the window to close
//! and then reads the whole set twice: two identical sets cannot have
//! straddled an update, which is what makes the reading coherent
//! without stopping the clock.
//!
//! Concurrency contract: the kernel reads the bank once, on the
//! bootstrap processor, before secondaries start and before the
//! executor runs. Nothing else in this backend touches these ports, so
//! the index register needs no lock.

use acpi::sdt::fadt::Fadt;
use helios_hal::rtc::{CalendarEncoding, RawCalendar, RealTimeClock, RtcError, UnixSeconds};
use x86_64::instructions::port::{Port, PortWriteOnly};

const CMOS_INDEX_PORT: u16 = 0x70;
const CMOS_DATA_PORT: u16 = 0x71;

const REGISTER_SECOND: u8 = 0x00;
const REGISTER_MINUTE: u8 = 0x02;
const REGISTER_HOUR: u8 = 0x04;
const REGISTER_DAY: u8 = 0x07;
const REGISTER_MONTH: u8 = 0x08;
const REGISTER_YEAR: u8 = 0x09;
const REGISTER_STATUS_A: u8 = 0x0a;
const REGISTER_STATUS_B: u8 = 0x0b;

/// Status A bit 7: the bank is partway through its once-a-second update
/// and its registers must not be read.
const STATUS_A_UPDATE_IN_PROGRESS: u8 = 0x80;
/// Status B bit 1: hours run 0..=23 instead of 1..=12.
const STATUS_B_TWENTY_FOUR_HOUR: u8 = 0x02;
/// Status B bit 2: registers hold plain binary instead of BCD.
const STATUS_B_BINARY: u8 = 0x04;

/// How many times a read may see the register set change under it
/// before the clock is declared unreadable. An update takes under two
/// milliseconds and happens once a second, so a second disagreement is
/// already a broken device rather than bad luck.
const SETTLE_ATTEMPTS: u32 = 4;

/// How long the update window is waited out before the bank is declared
/// stuck. The window lasts under 2 ms; the bank is read on the
/// bootstrap processor before the executor exists, so there is no task
/// to yield to and the wait is a bounded spin.
const UPDATE_WINDOW_SPINS: u32 = 1_000_000;

#[derive(Clone, Copy)]
pub(crate) struct CmosRtc {
    /// The century register the FADT points at, absent on a platform
    /// that declares none.
    century_register: Option<u8>,
}

impl RealTimeClock for CmosRtc {
    const SOURCE: &'static str = "mc146818-cmos";

    fn read(&self) -> Result<UnixSeconds, RtcError> {
        let status_b = read_register(REGISTER_STATUS_B);
        let encoding = CalendarEncoding {
            binary: status_b & STATUS_B_BINARY != 0,
            twenty_four_hour: status_b & STATUS_B_TWENTY_FOUR_HOUR != 0,
        };

        let mut previous = self.read_registers()?;
        for _ in 0..SETTLE_ATTEMPTS {
            let current = self.read_registers()?;
            if current == previous {
                return current.decode(encoding, DEFAULT_CENTURY)?.to_unix_seconds();
            }
            previous = current;
        }
        Err(RtcError::Unsettled {
            attempts: SETTLE_ATTEMPTS,
        })
    }
}

/// The century a platform that points at no century register is taken
/// to be in. Every machine this kernel boots on ships in the twenty
/// first century, and a platform that outlives it publishes the
/// register instead.
const DEFAULT_CENTURY: u8 = 20;

impl CmosRtc {
    fn read_registers(&self) -> Result<RawCalendar, RtcError> {
        wait_for_update_window()?;
        Ok(RawCalendar {
            second: read_register(REGISTER_SECOND),
            minute: read_register(REGISTER_MINUTE),
            hour: read_register(REGISTER_HOUR),
            day: read_register(REGISTER_DAY),
            month: read_register(REGISTER_MONTH),
            year: read_register(REGISTER_YEAR),
            century: self.century_register.map(read_register),
        })
    }
}

/// Reads the platform's calendar clock out of the ACPI tables.
///
/// A platform whose FADT says the CMOS ports must not be driven — a
/// machine with no such bank, or one that moved it — has no clock this
/// driver can read, and the caller leaves the kernel's wall clock
/// unseeded.
pub(crate) fn discover(rsdp_address: usize, physical_memory_offset: usize) -> Option<CmosRtc> {
    let handler = crate::smp::PhysicalOffsetAcpiHandler {
        physical_memory_offset,
        tsc_base: 0,
        tsc_hz: 1,
    };
    let tables = unsafe { acpi::AcpiTables::from_rsdp(handler, rsdp_address) }
        .unwrap_or_else(|error| panic!("failed to parse ACPI tables for the FADT: {error:?}"));
    let fadt = tables.find_table::<Fadt>()?;
    // `Fadt` is a packed structure, so the fields are copied out before
    // anything takes a reference to them.
    let boot_architecture = fadt.iapc_boot_arch;
    if boot_architecture.use_time_and_alarm_namespace_for_rtc() {
        return None;
    }
    let century = fadt.century;
    Some(CmosRtc {
        // The FADT writes zero for "this platform has no century
        // register", which is not a register index.
        century_register: (century != 0).then_some(century),
    })
}

fn read_register(register: u8) -> u8 {
    // SAFETY: the CMOS index/data pair is a fixed ISA port pair, and
    // this backend drives it from the bootstrap processor only, so no
    // other reader can observe the index between the two accesses.
    unsafe {
        PortWriteOnly::new(CMOS_INDEX_PORT).write(register);
        Port::<u8>::new(CMOS_DATA_PORT).read()
    }
}

/// Waits out the bank's once-a-second update window.
fn wait_for_update_window() -> Result<(), RtcError> {
    for _ in 0..UPDATE_WINDOW_SPINS {
        if read_register(REGISTER_STATUS_A) & STATUS_A_UPDATE_IN_PROGRESS == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(RtcError::Unsettled {
        attempts: UPDATE_WINDOW_SPINS,
    })
}
