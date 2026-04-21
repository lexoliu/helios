use fdt::Fdt;

use crate::pci::PciHostController;

pub(crate) type RiscvWatchdog = i6300esb::I6300EsbWatchdog<crate::pci::EcamPciConfigAccess>;

pub(crate) fn discover(fdt: &Fdt<'_>) -> RiscvWatchdog {
    let Some(pci) = PciHostController::discover(fdt) else {
        return i6300esb::disabled();
    };
    let access = pci.config_access();
    let bus_range = pci.bus_range();
    if let Some(address) = i6300esb::find_i6300esb(&access, bus_range.clone()) {
        pci.assign_unassigned_memory_bars(address);
    }
    i6300esb::discover_watchdog(access, bus_range, |bus_address| {
        pci.translate_mmio_address(bus_address)
    })
}
