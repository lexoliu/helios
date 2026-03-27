pub trait KernelResource: Clone {
    type Rights;
    fn rights(&self) -> Self::Rights;

    /// Derive a new resource with the given rights. The derived resource must have rights that are a subset of the original resource's rights.
    fn derive(&self, rights: Self::Rights) -> Option<Self>;
}
