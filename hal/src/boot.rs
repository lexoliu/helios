#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootMemoryKind {
    Usable,
    Reserved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootMemoryRegion {
    pub start: u64,
    pub end: u64,
    pub kind: BootMemoryKind,
}

impl BootMemoryRegion {
    pub const fn usable(self) -> bool {
        matches!(self.kind, BootMemoryKind::Usable)
    }
}

pub trait BootMemoryMap {
    type Iter: Iterator<Item = BootMemoryRegion>;

    fn regions(&self) -> Self::Iter;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirmwareKind {
    Uefi32,
    Uefi64,
    Sbi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootKernelImage {
    pub physical_base: u64,
    pub virtual_base: u64,
    pub file_address: usize,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootFirmwareTables {
    pub acpi_rsdp: Option<usize>,
    pub device_tree_blob: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootModule<'a> {
    pub address: usize,
    pub size: usize,
    pub path: &'a [u8],
    pub command_line: &'a [u8],
}

pub trait BootModules<'a> {
    type Iter: Iterator<Item = BootModule<'a>>;

    fn modules(&self) -> Self::Iter;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoBootModules;

impl<'a> BootModules<'a> for NoBootModules {
    type Iter = core::iter::Empty<BootModule<'a>>;

    fn modules(&self) -> Self::Iter {
        core::iter::empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootHandoff<'a, MemoryMap, Modules> {
    pub memory_map: MemoryMap,
    pub kernel: BootKernelImage,
    pub command_line: &'a [u8],
    pub modules: Modules,
    pub firmware: FirmwareKind,
    pub tables: BootFirmwareTables,
}
