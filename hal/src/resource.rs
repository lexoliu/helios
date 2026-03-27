/// Capability set carried by a kernel resource.
///
/// The kernel only needs one operation at this layer: checking whether one
/// capability set is a subset of another. Concrete backends can represent
/// rights with bitflags or any other compact value as long as they implement
/// this relation.
pub trait ResourceRights: Copy + Eq {
    fn contains(self, other: Self) -> bool;
}

/// Cloneable kernel object with an explicit, derivable capability set.
///
/// Implementors only provide the unchecked "same object, narrower rights"
/// operation. The safe `derive` entry point lives here so every resource type
/// gets the same subset check and cannot accidentally forget it.
pub trait KernelResource: Clone {
    type Rights: ResourceRights;

    fn rights(&self) -> Self::Rights;

    /// Clones the same underlying resource with a new capability set.
    ///
    /// # Safety
    ///
    /// `rights` must be a subset of `self.rights()`. Callers should use
    /// [`KernelResource::derive`] instead of invoking this directly.
    unsafe fn clone_with_rights(&self, rights: Self::Rights) -> Self;

    fn derive(&self, rights: Self::Rights) -> Option<Self> {
        self.rights()
            .contains(rights)
            .then(|| unsafe { self.clone_with_rights(rights) })
    }
}
