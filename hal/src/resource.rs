use bitflags::bitflags;
use triomphe::Arc;

/// Capability set carried by a kernel resource.
///
/// The kernel only needs one operation at this layer: checking whether one
/// capability set is a subset of another. Concrete backends can represent
/// rights with bitflags or any other compact value as long as they implement
/// this relation.
pub trait ResourceRights: Copy + Eq {
    fn contains(self, other: Self) -> bool;
}

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

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq)]
    /// Rights for a serial transport capability.
    pub struct SerialRights:u8{
        const READ  = 1 << 0;
        const WRITE = 1 << 1;
        const FLUSH = 1 << 2;
    }
}

impl ResourceRights for WasiRights {
    fn contains(self, other: Self) -> bool {
        (self.bits() & other.bits()) == other.bits()
    }
}

impl ResourceRights for SerialRights {
    fn contains(self, other: Self) -> bool {
        (self.bits() & other.bits()) == other.bits()
    }
}

/// Reference-counted capability handle to a kernel object.
///
/// The handle owns a strong reference to the underlying object and carries the
/// rights exposed through this particular view. Deriving a new handle can only
/// narrow rights; the backing object is shared through RAII and drops once the
/// last handle is released.
pub struct KernelResource<T, Rights>
where
    Rights: ResourceRights,
{
    object: Arc<T>,
    rights: Rights,
}

impl<T, Rights> KernelResource<T, Rights>
where
    Rights: ResourceRights,
{
    pub fn new(object: T, rights: Rights) -> Self {
        Self {
            object: Arc::new(object),
            rights,
        }
    }

    pub fn from_arc(object: Arc<T>, rights: Rights) -> Self {
        Self { object, rights }
    }

    pub fn rights(&self) -> Rights {
        self.rights
    }

    pub fn derive(&self, rights: Rights) -> Option<Self> {
        self.rights.contains(rights).then(|| Self {
            object: self.object.clone(),
            rights,
        })
    }

    pub fn object(&self) -> &T {
        &self.object
    }

    pub fn into_arc(self) -> Arc<T> {
        self.object
    }

    pub fn as_arc(&self) -> &Arc<T> {
        &self.object
    }
}

impl<T, Rights> Clone for KernelResource<T, Rights>
where
    Rights: ResourceRights,
{
    fn clone(&self) -> Self {
        Self {
            object: self.object.clone(),
            rights: self.rights,
        }
    }
}

impl<T, Rights> AsRef<T> for KernelResource<T, Rights>
where
    Rights: ResourceRights,
{
    fn as_ref(&self) -> &T {
        self.object()
    }
}

pub type SerialResource<T> = KernelResource<T, SerialRights>;
