use helios_hal::boot::{
    BootFirmwareTables, BootHandoff, BootKernelImage, BootMemoryKind, BootMemoryMap,
    BootMemoryRegion, BootModule, BootModules, FirmwareKind,
};
use limine::BaseRevision;
use limine::firmware_type::FirmwareType;
use limine::memory_map::{Entry, EntryType};
use limine::request::{
    DeviceTreeBlobRequest, ExecutableAddressRequest, ExecutableCmdlineRequest,
    ExecutableFileRequest, FirmwareTypeRequest, HhdmRequest, MemoryMapRequest, ModuleRequest,
    RsdpRequest, StackSizeRequest,
};

use crate::KERNEL_STACK_BYTES;

#[used]
static BASE_REVISION: BaseRevision = BaseRevision::new();
#[used]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();
#[used]
static MEMORY_MAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();
#[used]
static EXECUTABLE_ADDRESS_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();
#[used]
static EXECUTABLE_FILE_REQUEST: ExecutableFileRequest = ExecutableFileRequest::new();
#[used]
static EXECUTABLE_CMDLINE_REQUEST: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();
#[used]
static MODULE_REQUEST: ModuleRequest = ModuleRequest::new();
#[used]
static FIRMWARE_TYPE_REQUEST: FirmwareTypeRequest = FirmwareTypeRequest::new();
#[used]
static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();
#[used]
static DEVICE_TREE_BLOB_REQUEST: DeviceTreeBlobRequest = DeviceTreeBlobRequest::new();
#[used]
static STACK_SIZE_REQUEST: StackSizeRequest =
    StackSizeRequest::new().with_size(KERNEL_STACK_BYTES as u64);

pub(crate) fn base_revision_supported() -> bool {
    BASE_REVISION.is_supported()
}

pub(crate) fn physical_memory_offset() -> usize {
    HHDM_REQUEST
        .get_response()
        .unwrap_or_else(|| panic!("Limine did not provide an HHDM response"))
        .offset() as usize
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LimineMemoryMap {
    entries: &'static [&'static Entry],
}

impl BootMemoryMap for LimineMemoryMap {
    type Iter = core::iter::Map<
        core::iter::Copied<core::slice::Iter<'static, &'static Entry>>,
        fn(&'static Entry) -> BootMemoryRegion,
    >;

    fn regions(&self) -> Self::Iter {
        self.entries.iter().copied().map(convert_memory_region)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LimineModules {
    files: &'static [&'static limine::file::File],
}

impl<'a> BootModules<'a> for LimineModules {
    type Iter = core::iter::Map<
        core::iter::Copied<core::slice::Iter<'static, &'static limine::file::File>>,
        fn(&'static limine::file::File) -> BootModule<'static>,
    >;

    fn modules(&self) -> Self::Iter {
        self.files.iter().copied().map(convert_module)
    }
}

pub(crate) type LimineBootHandoff =
    BootHandoff<'static, LimineMemoryMap, LimineModules>;

pub(crate) fn limine_boot_handoff() -> LimineBootHandoff {
    let memory_map = LimineMemoryMap {
        entries: MEMORY_MAP_REQUEST
            .get_response()
            .unwrap_or_else(|| panic!("Limine did not provide a memory map response"))
            .entries(),
    };
    let executable_address = EXECUTABLE_ADDRESS_REQUEST
        .get_response()
        .unwrap_or_else(|| panic!("Limine did not provide an executable address response"));
    let executable_file = EXECUTABLE_FILE_REQUEST
        .get_response()
        .unwrap_or_else(|| panic!("Limine did not provide an executable file response"))
        .file();
    let command_line = EXECUTABLE_CMDLINE_REQUEST
        .get_response()
        .unwrap_or_else(|| panic!("Limine did not provide an executable command-line response"))
        .cmdline()
        .to_bytes();
    let firmware = FIRMWARE_TYPE_REQUEST
        .get_response()
        .unwrap_or_else(|| panic!("Limine did not provide a firmware type response"))
        .firmware_type();
    let firmware = convert_firmware_kind(firmware);
    assert!(
        firmware == FirmwareKind::Uefi64,
        "x86_64 backend requires Limine UEFI64 boot, got {firmware:?}"
    );
    let acpi_rsdp = RSDP_REQUEST
        .get_response()
        .unwrap_or_else(|| panic!("Limine did not provide an RSDP response"))
        .address();
    let device_tree_blob = DEVICE_TREE_BLOB_REQUEST
        .get_response()
        .map(|response| response.dtb_ptr() as usize);
    BootHandoff {
        memory_map,
        kernel: BootKernelImage {
            physical_base: executable_address.physical_base(),
            virtual_base: executable_address.virtual_base(),
            file_address: executable_file.addr() as usize,
            size: executable_file.size(),
        },
        command_line,
        modules: LimineModules {
            files: MODULE_REQUEST
                .get_response()
                .map(|response| response.modules())
                .unwrap_or(&[]),
        },
        firmware,
        tables: BootFirmwareTables {
            acpi_rsdp: Some(acpi_rsdp),
            device_tree_blob,
        },
    }
}

fn convert_memory_region(entry: &'static Entry) -> BootMemoryRegion {
    BootMemoryRegion {
        start: entry.base,
        end: entry.base + entry.length,
        kind: convert_memory_kind(entry.entry_type),
    }
}

fn convert_memory_kind(entry_type: EntryType) -> BootMemoryKind {
    if entry_type == EntryType::USABLE {
        BootMemoryKind::Usable
    } else {
        BootMemoryKind::Reserved
    }
}

fn convert_module(file: &'static limine::file::File) -> BootModule<'static> {
    BootModule {
        address: file.addr() as usize,
        size: usize::try_from(file.size()).unwrap_or_else(|_| panic!("Limine module is too large")),
        path: file.path().to_bytes(),
        command_line: file.string().to_bytes(),
    }
}

fn convert_firmware_kind(firmware: FirmwareType) -> FirmwareKind {
    if firmware == FirmwareType::UEFI_32 {
        FirmwareKind::Uefi32
    } else if firmware == FirmwareType::UEFI_64 {
        FirmwareKind::Uefi64
    } else if firmware == FirmwareType::SBI {
        FirmwareKind::Sbi
    } else if firmware == FirmwareType::X86_BIOS {
        panic!("Limine BIOS boot is not supported")
    } else {
        panic!("Limine reported an unknown firmware type")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_limine_memory_kinds_to_handoff_kinds() {
        assert_eq!(
            convert_memory_kind(EntryType::USABLE),
            BootMemoryKind::Usable
        );
        assert_eq!(
            convert_memory_kind(EntryType::BOOTLOADER_RECLAIMABLE),
            BootMemoryKind::Reserved
        );
    }

    #[test]
    #[test]
    #[should_panic(expected = "Limine BIOS boot is not supported")]
    fn rejects_limine_bios_firmware() {
        let _ = convert_firmware_kind(FirmwareType::X86_BIOS);
    }
}
