use bitflags::bitflags;
use helios_hal::resource::ResourceRights;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq)]
    /// WASI-style rights for a resource.
    pub struct WasiRights:u64{
        const FD_DATASYNC           = 1 << 0;
        const FD_READ               = 1 << 1;
        const FD_SEEK               = 1 << 2;
        const FD_FDSTAT_SET_FLAGS   = 1 << 3;
        const FD_SYNC               = 1 << 4;
        const FD_TELL               = 1 << 5;
        const FD_WRITE              = 1 << 6;
        const FD_ADVISE             = 1 << 7;
        const FD_ALLOCATE           = 1 << 8;
    }
}

impl ResourceRights for WasiRights {
    fn contains(self, other: Self) -> bool {
        (self.bits() & other.bits()) == other.bits()
    }
}
