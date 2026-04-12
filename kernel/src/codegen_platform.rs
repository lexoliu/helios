/// Runtime code-generation platform support.
///
/// Backends that host a JIT-compiling wasm runtime implement this trait to
/// provide architecture-specific code publication and ISA feature probing.
/// This is intentionally separate from [`helios_hal::cpu::Cpu`] because JIT
/// support is a runtime concern, not a hardware abstraction.
pub trait CodegenPlatform: Send + Sync + 'static {
    /// Publishes freshly JIT-compiled executable code so that it becomes
    /// visible to instruction fetches on all processors.
    ///
    /// Architectures that require explicit instruction-cache synchronisation
    /// (RISC-V `fence.i`, ARM `dc cvau` + `ic ivau` + `isb`) or page-table
    /// permission changes (x86 clearing the NX bit) implement the appropriate
    /// sequence here. Backends without such requirements may leave this empty.
    fn publish_executable(&self, ptr: *const u8, len: usize);

    /// Returns an optional native ISA feature probe.
    ///
    /// A JIT backend can call the returned function with feature names (e.g.
    /// `"avx2"`, `"bmi2"`) to discover what the current platform supports
    /// without hardcoding architecture detection logic into the kernel.
    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        None
    }
}
