#![no_std]

use core::num::NonZeroU16;
use core::ops::RangeInclusive;
use core::time::Duration;

use helios_hal::watchdog::{ProgressCounter, Watchdog};
use pci_types::{CommandRegister, ConfigRegionAccess, EndpointHeader, PciAddress, PciHeader};

pub const INTEL_VENDOR_ID: u16 = 0x8086;
pub const I6300ESB_DEVICE_ID: u16 = 0x25ab;
pub const DEFAULT_WATCHDOG_TIMEOUT_SECONDS: u16 = 30;
const WATCHDOG_TIMEOUT_SECONDS_ENV: Option<&str> = option_env!("HELIOS_WATCHDOG_TIMEOUT_SECS");

const ESB_CONFIG_REG: u16 = 0x60;
const ESB_LOCK_REG: u16 = 0x68;
const ESB_TIMER1_REG: usize = 0x00;
const ESB_TIMER2_REG: usize = 0x04;
const ESB_RELOAD_REG: usize = 0x0c;
const ESB_DISABLE_TIMER1_INTERRUPT: u16 = 0x0003;
const ESB_WDT_ENABLE: u8 = 0x01 << 1;
const ESB_WDT_TIMEOUT: u16 = 0x01 << 9;
const ESB_WDT_RELOAD: u16 = 0x01 << 8;
const ESB_UNLOCK1: u16 = 0x80;
const ESB_UNLOCK2: u16 = 0x86;
const PCI_MAX_DEVICE: u8 = 32;
const PCI_MAX_FUNCTION: u8 = 8;

pub trait PciConfigIo: ConfigRegionAccess {
    fn read_u8(&self, address: PciAddress, offset: u16) -> u8;
    fn read_u16(&self, address: PciAddress, offset: u16) -> u16;
    fn write_u8(&self, address: PciAddress, offset: u16, value: u8);
    fn write_u16(&self, address: PciAddress, offset: u16, value: u16);
}

#[derive(Clone)]
pub struct I6300EsbWatchdog<Access>
where
    Access: PciConfigIo + Clone + Send + Sync + 'static,
{
    timeout: Duration,
    progress: ProgressCounter,
    device: Option<I6300EsbDevice<Access>>,
}

#[derive(Clone)]
struct I6300EsbDevice<Access>
where
    Access: PciConfigIo + Clone + Send + Sync + 'static,
{
    access: Access,
    address: PciAddress,
    base_address: usize,
}

pub fn discover_watchdog<Access, Translate>(
    access: Access,
    buses: RangeInclusive<u8>,
    translate_bar_address: Translate,
) -> I6300EsbWatchdog<Access>
where
    Access: PciConfigIo + Clone + Send + Sync + 'static,
    Translate: Fn(usize) -> usize,
{
    let Some(address) = find_i6300esb(&access, buses) else {
        return disabled();
    };

    let timeout = configured_timeout_seconds();
    let bar0 = unsafe { access.read(address, 0x10) };
    assert!(
        bar0 & 0x1 == 0,
        "i6300esb BAR0 unexpectedly used I/O space at {address}"
    );
    let bus_address = (bar0 as usize) & !0x0f;
    assert!(
        bus_address != 0,
        "i6300esb BAR0 was not assigned a memory base address"
    );
    let base_address = translate_bar_address(bus_address);
    let device = I6300EsbDevice {
        access,
        address,
        base_address,
    };
    device.initialize(timeout);

    I6300EsbWatchdog {
        timeout: Duration::from_secs(u64::from(timeout.get())),
        progress: ProgressCounter::new(),
        device: Some(device),
    }
}

pub fn disabled<Access>() -> I6300EsbWatchdog<Access>
where
    Access: PciConfigIo + Clone + Send + Sync + 'static,
{
    I6300EsbWatchdog {
        timeout: Duration::ZERO,
        progress: ProgressCounter::new(),
        device: None,
    }
}

pub fn find_i6300esb<Access>(access: &Access, buses: RangeInclusive<u8>) -> Option<PciAddress>
where
    Access: PciConfigIo,
{
    for bus in buses {
        for device in 0..PCI_MAX_DEVICE {
            let slot = PciAddress::new(0, bus, device, 0);
            let header = PciHeader::new(slot);
            let (vendor, _) = header.id(access);
            if vendor == u16::MAX {
                continue;
            }

            let function_count = if header.has_multiple_functions(access) {
                PCI_MAX_FUNCTION
            } else {
                1
            };
            for function in 0..function_count {
                let address = PciAddress::new(0, bus, device, function);
                let header = PciHeader::new(address);
                let (vendor, device_id) = header.id(access);
                if vendor == u16::MAX {
                    continue;
                }
                if vendor == INTEL_VENDOR_ID && device_id == I6300ESB_DEVICE_ID {
                    EndpointHeader::from_header(header, access).unwrap_or_else(|| {
                        panic!("PCI function {address} matched i6300esb id but was not an endpoint")
                    });
                    return Some(address);
                }
            }
        }
    }

    None
}

impl<Access> Watchdog for I6300EsbWatchdog<Access>
where
    Access: PciConfigIo + Clone + Send + Sync + 'static,
{
    fn progress(&self) -> ProgressCounter {
        self.progress.clone()
    }

    fn is_enabled(&self) -> bool {
        self.device.is_some()
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    fn arm(&self) {
        let Some(device) = &self.device else {
            return;
        };
        device.arm(self.timeout);
    }

    fn pet(&self) {
        let Some(device) = &self.device else {
            return;
        };
        device.pet();
    }

    fn disarm(&self) {
        let Some(device) = &self.device else {
            return;
        };
        device.disarm();
    }
}

impl<Access> I6300EsbDevice<Access>
where
    Access: PciConfigIo + Clone + Send + Sync + 'static,
{
    fn initialize(&self, timeout: NonZeroU16) {
        let command = self.access.read_u16(self.address, 0x04);
        self.access.write_u16(
            self.address,
            0x04,
            command | CommandRegister::MEMORY_ENABLE.bits(),
        );
        self.access
            .write_u16(self.address, ESB_CONFIG_REG, ESB_DISABLE_TIMER1_INTERRUPT);
        assert!(
            self.access.read_u16(self.address, ESB_CONFIG_REG) == ESB_DISABLE_TIMER1_INTERRUPT,
            "i6300esb config register write did not stick"
        );
        self.disarm();
        self.clear_timeout_latched();
        self.program_timeout(timeout);
    }

    fn arm(&self, timeout: Duration) {
        let timeout_seconds = timeout
            .as_secs()
            .try_into()
            .ok()
            .and_then(NonZeroU16::new)
            .unwrap_or_else(|| panic!("watchdog timeout {timeout:?} does not fit in i6300esb"));
        self.program_timeout(timeout_seconds);
        self.pet();
        self.access
            .write_u8(self.address, ESB_LOCK_REG, ESB_WDT_ENABLE);
        assert!(
            self.access.read_u8(self.address, ESB_LOCK_REG) & ESB_WDT_ENABLE != 0,
            "i6300esb enable bit did not stick"
        );
    }

    fn pet(&self) {
        critical_section::with(|_| {
            self.unlock_registers();
            self.write_reload(ESB_WDT_RELOAD);
        });
        self.flush_posted_writes();
    }

    fn disarm(&self) {
        critical_section::with(|_| {
            self.unlock_registers();
            self.write_reload(ESB_WDT_RELOAD);
            self.access.write_u8(self.address, ESB_LOCK_REG, 0);
        });
        self.flush_posted_writes();
    }

    fn program_timeout(&self, timeout: NonZeroU16) {
        let seconds = u32::from(timeout.get());
        assert!(
            seconds <= 2 * 0x03ff,
            "i6300esb timeout must be in 1..=2046 seconds, got {seconds}"
        );
        let reload_ticks = seconds << 9;
        critical_section::with(|_| {
            self.unlock_registers();
            self.write_timer(ESB_TIMER1_REG, reload_ticks);
            self.unlock_registers();
            self.write_timer(ESB_TIMER2_REG, reload_ticks);
            self.unlock_registers();
            self.write_reload(ESB_WDT_RELOAD);
        });
        self.flush_posted_writes();
    }

    fn clear_timeout_latched(&self) {
        critical_section::with(|_| {
            self.unlock_registers();
            self.write_reload(ESB_WDT_TIMEOUT | ESB_WDT_RELOAD);
        });
    }

    fn unlock_registers(&self) {
        self.write_reload(ESB_UNLOCK1);
        self.write_reload(ESB_UNLOCK2);
    }

    fn write_timer(&self, offset: usize, value: u32) {
        let address = (self.base_address + offset) as *mut u32;
        unsafe { address.write_volatile(value) }
    }
    fn write_reload(&self, value: u16) {
        let address = (self.base_address + ESB_RELOAD_REG) as *mut u16;
        unsafe { address.write_volatile(value) }
    }

    fn flush_posted_writes(&self) {
        let _ = self.access.read_u8(self.address, ESB_LOCK_REG);
    }
}

fn configured_timeout_seconds() -> NonZeroU16 {
    let raw = WATCHDOG_TIMEOUT_SECONDS_ENV.unwrap_or("30");
    let seconds = raw
        .parse::<u16>()
        .unwrap_or_else(|error| panic!("invalid HELIOS_WATCHDOG_TIMEOUT_SECS={raw:?}: {error}"));
    NonZeroU16::new(seconds)
        .unwrap_or_else(|| panic!("HELIOS_WATCHDOG_TIMEOUT_SECS must be non-zero"))
}
