extern crate alloc;

use alloc::sync::Arc;
use core::num::NonZeroU16;
use core::time::Duration;

use helios_hal::watchdog::{ProgressCounter, Watchdog};
use pci_types::{CommandRegister, ConfigRegionAccess, PciHeader};

use crate::pci::LegacyPciConfigAccess;

const INTEL_VENDOR_ID: u16 = 0x8086;
const I6300ESB_DEVICE_ID: u16 = 0x25ab;
const DEFAULT_WATCHDOG_TIMEOUT_SECONDS: u16 = 30;
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

#[derive(Clone)]
pub(crate) struct X86Watchdog {
    timeout: Duration,
    progress: ProgressCounter,
    device: Option<Arc<I6300EsbDevice>>,
}

struct I6300EsbDevice {
    address: pci_types::PciAddress,
    base_address: usize,
}

impl X86Watchdog {
    pub(crate) fn discover(physical_memory_offset: usize) -> Self {
        let progress = ProgressCounter::new();
        let access = LegacyPciConfigAccess::new();
        let Some(endpoint) = access.find_endpoint(INTEL_VENDOR_ID, I6300ESB_DEVICE_ID) else {
            return Self {
                timeout: Duration::ZERO,
                progress,
                device: None,
            };
        };

        let timeout_seconds = NonZeroU16::new(DEFAULT_WATCHDOG_TIMEOUT_SECONDS)
            .unwrap_or_else(|| panic!("x86 watchdog timeout must be non-zero"));
        let address = endpoint.header().address();
        let bar0 = unsafe { access.read(address, 0x10) };
        assert!(
            bar0 & 0x1 == 0,
            "i6300esb BAR0 unexpectedly used I/O space at {address}"
        );
        let physical_base = (bar0 as usize) & !0x0f;
        assert!(
            physical_base != 0,
            "i6300esb BAR0 was not assigned a memory base address"
        );
        let base_address = physical_memory_offset
            .checked_add(physical_base)
            .unwrap_or_else(|| panic!("i6300esb BAR0 virtual address overflow"));
        let device = Arc::new(I6300EsbDevice {
            address,
            base_address,
        });
        device.initialize(timeout_seconds);

        Self {
            timeout: Duration::from_secs(u64::from(timeout_seconds.get())),
            progress,
            device: Some(device),
        }
    }
}

impl Watchdog for X86Watchdog {
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

impl I6300EsbDevice {
    fn initialize(&self, timeout: NonZeroU16) {
        let access = LegacyPciConfigAccess::new();
        let mut header = PciHeader::new(self.address);
        header.update_command(access, |command| command | CommandRegister::MEMORY_ENABLE);
        access.write_u16(self.address, ESB_CONFIG_REG, ESB_DISABLE_TIMER1_INTERRUPT);
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
        LegacyPciConfigAccess::new().write_u8(self.address, ESB_LOCK_REG, ESB_WDT_ENABLE);
    }

    fn pet(&self) {
        critical_section::with(|_| {
            self.unlock_registers();
            self.write_reload(ESB_WDT_RELOAD);
        });
    }

    fn disarm(&self) {
        critical_section::with(|_| {
            self.unlock_registers();
            self.write_reload(ESB_WDT_RELOAD);
            LegacyPciConfigAccess::new().write_u8(self.address, ESB_LOCK_REG, 0);
        });
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
        unsafe { ((self.base_address + offset) as *mut u32).write_volatile(value) }
    }

    fn write_reload(&self, value: u16) {
        let address = (self.base_address + ESB_RELOAD_REG) as *mut u16;
        unsafe { address.write_volatile(value) }
    }
}
