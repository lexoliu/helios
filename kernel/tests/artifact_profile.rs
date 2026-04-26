use helios_kernel::{ArtifactKind, ArtifactProfile, ArtifactProfileError, classify_raw_wasm};
use wasm_encoder::{
    Component, ComponentImportSection, ComponentTypeRef, EntityType, ImportSection, MemoryType,
    Module,
};

#[test]
fn classifies_preview1_core_module() {
    let mut imports = ImportSection::new();
    imports.import(
        "wasi_snapshot_preview1",
        "args_sizes_get",
        EntityType::Function(0),
    );
    let mut module = Module::new();
    module.section(&wasm_encoder::TypeSection::new());
    module.section(&imports);

    let report = classify_raw_wasm(&module.finish()).unwrap();
    assert_eq!(report.kind, ArtifactKind::CoreModule);
    assert_eq!(report.profile, ArtifactProfile::CorePreview1);
}

#[test]
fn classifies_preview1_threads_core_module() {
    let mut imports = ImportSection::new();
    imports.import(
        "wasi_snapshot_preview1",
        "sched_yield",
        EntityType::Function(0),
    );
    imports.import("wasi", "thread-spawn", EntityType::Function(0));
    imports.import(
        "env",
        "memory",
        EntityType::Memory(MemoryType {
            minimum: 1,
            maximum: Some(1),
            memory64: false,
            shared: true,
            page_size_log2: None,
        }),
    );
    let mut module = Module::new();
    module.section(&wasm_encoder::TypeSection::new());
    module.section(&imports);

    let report = classify_raw_wasm(&module.finish()).unwrap();
    assert_eq!(report.kind, ArtifactKind::CoreModule);
    assert_eq!(report.profile, ArtifactProfile::CorePreview1Threads);
}

#[test]
fn rejects_unknown_core_imports() {
    let mut imports = ImportSection::new();
    imports.import("host", "call", EntityType::Function(0));
    let mut module = Module::new();
    module.section(&wasm_encoder::TypeSection::new());
    module.section(&imports);

    let error = classify_raw_wasm(&module.finish()).unwrap_err();
    assert!(matches!(
        error,
        ArtifactProfileError::UnknownCoreImport { .. }
    ));
}

#[test]
fn classifies_preview2_component() {
    let mut imports = ComponentImportSection::new();
    imports.import("wasi:cli/environment@0.3.0", ComponentTypeRef::Instance(0));
    let mut component = Component::new();
    component.section(&imports);

    let report = classify_raw_wasm(&component.finish()).unwrap();
    assert_eq!(report.kind, ArtifactKind::Component);
    assert_eq!(report.profile, ArtifactProfile::ComponentPreview2);
}
