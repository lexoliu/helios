extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use thiserror::Error;
use wasmparser::{Encoding, Import, Imports, Parser, Payload, TypeRef};

const WASI_PREVIEW1_MODULE: &str = "wasi_snapshot_preview1";
const WASI_THREADS_MODULE: &str = "wasi";
const WASI_THREAD_SPAWN: &str = "thread-spawn";
const WASIX_MODULE: &str = "wasix_32v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactKind {
    CoreModule,
    Component,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactProfile {
    CorePreview1,
    CorePreview1Threads,
    CoreWasix,
    ComponentPreview2,
    ComponentPreview3,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactImport {
    pub module: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactProfileReport {
    pub kind: ArtifactKind,
    pub profile: ArtifactProfile,
    pub imports: Vec<ArtifactImport>,
}

#[derive(Debug, Error)]
pub enum ArtifactProfileError {
    #[error("failed to parse wasm artifact: {0}")]
    Parse(#[from] wasmparser::BinaryReaderError),
    #[error("wasm artifact did not contain a module or component header")]
    MissingHeader,
    #[error("core module artifact imports unsupported module `{module}` item `{name}`")]
    UnknownCoreImport { module: String, name: String },
    #[error("component artifact imports unsupported interface `{name}`")]
    UnknownComponentImport { name: String },
}

pub fn classify_raw_wasm(bytes: &[u8]) -> Result<ArtifactProfileReport, ArtifactProfileError> {
    let mut encoding = None;
    let mut core_imports = Vec::new();
    let mut component_imports = Vec::new();
    let mut has_shared_memory = false;

    for payload in Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::Version {
                encoding: parsed, ..
            } => {
                if encoding.is_none() {
                    encoding = Some(parsed);
                }
            }
            Payload::ImportSection(section) => {
                collect_core_imports(section, &mut core_imports, &mut has_shared_memory)?;
            }
            Payload::MemorySection(section) => {
                for memory in section {
                    has_shared_memory |= memory?.shared;
                }
            }
            Payload::ComponentImportSection(section) => {
                for import in section {
                    component_imports.push(import?.name.0.to_string());
                }
            }
            _ => {}
        }
    }

    match encoding.ok_or(ArtifactProfileError::MissingHeader)? {
        Encoding::Module => classify_core_module(core_imports, has_shared_memory),
        Encoding::Component => classify_component(component_imports),
    }
}

fn collect_core_imports(
    section: wasmparser::ImportSectionReader<'_>,
    imports: &mut Vec<ArtifactImport>,
    has_shared_memory: &mut bool,
) -> Result<(), ArtifactProfileError> {
    for group in section {
        match group? {
            Imports::Single(_, import) => {
                collect_core_import(import, imports, has_shared_memory);
            }
            Imports::Compact1 { module, items } => {
                for item in items {
                    let item = item?;
                    collect_core_import(
                        Import {
                            module,
                            name: item.name,
                            ty: item.ty,
                        },
                        imports,
                        has_shared_memory,
                    );
                }
            }
            Imports::Compact2 { module, ty, names } => {
                for name in names {
                    collect_core_import(
                        Import {
                            module,
                            name: name?,
                            ty,
                        },
                        imports,
                        has_shared_memory,
                    );
                }
            }
        }
    }
    Ok(())
}

fn collect_core_import(
    import: Import<'_>,
    imports: &mut Vec<ArtifactImport>,
    has_shared_memory: &mut bool,
) {
    if let TypeRef::Memory(memory) = import.ty {
        *has_shared_memory |= memory.shared;
        if memory.shared {
            return;
        }
    }
    imports.push(ArtifactImport {
        module: import.module.to_string(),
        name: import.name.to_string(),
    });
}

fn classify_core_module(
    imports: Vec<ArtifactImport>,
    has_shared_memory: bool,
) -> Result<ArtifactProfileReport, ArtifactProfileError> {
    let mut uses_preview1 = false;
    let mut uses_threads = has_shared_memory;
    let mut uses_wasix = false;

    for import in &imports {
        match (import.module.as_str(), import.name.as_str()) {
            (WASI_PREVIEW1_MODULE, _) => uses_preview1 = true,
            (WASI_THREADS_MODULE, WASI_THREAD_SPAWN) => uses_threads = true,
            (WASIX_MODULE, name) if super::wasix::authority_for(name).is_some() => {
                uses_wasix = true;
            }
            _ => {
                return Err(ArtifactProfileError::UnknownCoreImport {
                    module: import.module.clone(),
                    name: import.name.clone(),
                });
            }
        }
    }

    let profile = if uses_wasix {
        ArtifactProfile::CoreWasix
    } else if uses_threads {
        ArtifactProfile::CorePreview1Threads
    } else {
        ArtifactProfile::CorePreview1
    };

    if !uses_preview1 && !uses_wasix && !imports.is_empty() {
        let import = &imports[0];
        return Err(ArtifactProfileError::UnknownCoreImport {
            module: import.module.clone(),
            name: import.name.clone(),
        });
    }

    Ok(ArtifactProfileReport {
        kind: ArtifactKind::CoreModule,
        profile,
        imports,
    })
}

fn classify_component(imports: Vec<String>) -> Result<ArtifactProfileReport, ArtifactProfileError> {
    for import in &imports {
        if !is_supported_component_import(import) {
            return Err(ArtifactProfileError::UnknownComponentImport {
                name: import.clone(),
            });
        }
    }

    Ok(ArtifactProfileReport {
        kind: ArtifactKind::Component,
        profile: component_profile(&imports),
        imports: imports
            .into_iter()
            .map(|name| ArtifactImport {
                module: name,
                name: String::new(),
            })
            .collect(),
    })
}

fn component_profile(imports: &[String]) -> ArtifactProfile {
    if imports
        .iter()
        .any(|import| is_wasi_import_version(import, "0.3"))
    {
        ArtifactProfile::ComponentPreview3
    } else {
        ArtifactProfile::ComponentPreview2
    }
}

fn is_wasi_import_version(name: &str, version_prefix: &str) -> bool {
    name.starts_with("wasi:")
        && name
            .split_once('@')
            .is_some_and(|(_, version)| version.starts_with(version_prefix))
}

pub fn validate_component_import_name(name: &str) -> Result<(), ArtifactProfileError> {
    if is_supported_component_import(name) {
        Ok(())
    } else {
        Err(ArtifactProfileError::UnknownComponentImport {
            name: name.to_string(),
        })
    }
}

fn is_supported_component_import(name: &str) -> bool {
    name.starts_with("wasi:cli/")
        || name.starts_with("wasi:clocks/")
        || name.starts_with("wasi:filesystem/")
        || name.starts_with("wasi:io/")
        || name.starts_with("wasi:random/")
        || name.starts_with("wasi:sockets/")
        || name.starts_with("helios:system/")
        || name.starts_with("import-func-")
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use wasm_encoder::{
        Component, ComponentImportSection, ComponentTypeRef, EntityType, ImportSection, MemoryType,
        Module,
    };

    use super::{
        ArtifactKind, ArtifactProfile, ArtifactProfileError, classify_component, classify_raw_wasm,
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
    fn classifies_wasix_core_module() {
        let mut imports = ImportSection::new();
        imports.import("wasix_32v1", "fd_dup", EntityType::Function(0));
        imports.import("wasix_32v1", "proc_fork", EntityType::Function(0));
        let mut module = Module::new();
        module.section(&wasm_encoder::TypeSection::new());
        module.section(&imports);

        let report = classify_raw_wasm(&module.finish()).unwrap();
        assert_eq!(report.kind, ArtifactKind::CoreModule);
        assert_eq!(report.profile, ArtifactProfile::CoreWasix);
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
    fn component_imports_with_preview2_version_are_preview2() {
        let report = classify_component(alloc::vec![
            "wasi:cli/environment@0.2.6".to_string(),
            "wasi:filesystem/types@0.2.6".to_string(),
            "import-func-run".to_string(),
        ])
        .expect("Preview2 imports must classify");

        assert_eq!(report.profile, ArtifactProfile::ComponentPreview2);
    }

    #[test]
    fn component_imports_with_preview3_version_are_preview3() {
        let report = classify_component(alloc::vec![
            "wasi:cli/environment@0.3.0-rc-2026-03-15".to_string(),
            "wasi:filesystem/types@0.3.0-rc-2026-03-15".to_string(),
        ])
        .expect("Preview3 imports must classify");

        assert_eq!(report.profile, ArtifactProfile::ComponentPreview3);
    }

    #[test]
    fn classifies_preview3_component() {
        let mut imports = ComponentImportSection::new();
        imports.import("wasi:cli/environment@0.3.0", ComponentTypeRef::Instance(0));
        let mut component = Component::new();
        component.section(&imports);

        let report = classify_raw_wasm(&component.finish()).unwrap();
        assert_eq!(report.kind, ArtifactKind::Component);
        assert_eq!(report.profile, ArtifactProfile::ComponentPreview3);
    }
}
