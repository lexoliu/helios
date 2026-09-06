//! `anyhow` is banned from this workspace (AGENTS §3.2).
//!
//! Errors are typed enums built with `thiserror`, so a caller dispatches on a
//! stable contract and a failure keeps the provenance of the layer that
//! produced it. `anyhow` erases both: a `Context` chain is text, and the only
//! way back out of it is a downcast that names a type the compiler never
//! checked. A crate that pulls it in loses that, and does so quietly — the
//! dependency arrives in a manifest, not at a call site a reviewer reads.
//!
//! This test therefore reads the manifests rather than the sources: it is the
//! manifest that grants a crate the ability to reach for `anyhow` at all, and
//! refusing it there is what makes the ban hold for code nobody has written
//! yet.
//!
//! It lives beside the workspace-root resolver because that is the crate that
//! knows how to find the tree it is checking, and it walks every member of
//! that tree — no crate is exempt, including this one.

use std::fs;
use std::path::{Path, PathBuf};

use helios_workspace_root::WorkspaceRoot;

/// The dependency name no manifest in this workspace may carry.
const BANNED_DEPENDENCY: &str = "anyhow";

/// The manifest tables a dependency can be declared in.
///
/// A dev-dependency and a build-dependency are as much a part of the
/// contract as a normal one: a test that reaches for `anyhow` still writes
/// its assertions against untyped errors, and a build script still emits
/// diagnostics a maintainer has to read.
const DEPENDENCY_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

/// The `[target.'cfg(…)']` table, whose entries each hold their own
/// dependency tables.
const TARGET_TABLE: &str = "target";

#[test]
fn no_workspace_crate_depends_on_anyhow() {
    let root = WorkspaceRoot::discover_from(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|error| panic!("failed to find the workspace root: {error}"));

    let mut found = Vec::new();
    for manifest in workspace_manifests(root.path()) {
        for table in banned_declarations(&manifest) {
            found.push(format!(
                "{} declares {BANNED_DEPENDENCY} in [{table}]",
                manifest
                    .strip_prefix(root.path())
                    .unwrap_or(&manifest)
                    .display()
            ));
        }
    }

    assert!(
        found.is_empty(),
        "anyhow is banned from this workspace; use a typed thiserror enum instead:\n{}",
        found.join("\n")
    );
}

/// Every manifest the workspace owns: the root's own and each member's.
fn workspace_manifests(root: &Path) -> Vec<PathBuf> {
    let root_manifest = root.join("Cargo.toml");
    let table = parse_manifest(&root_manifest);
    let members = table
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| {
            panic!(
                "{} declares no [workspace] members",
                root_manifest.display()
            )
        });

    let mut manifests = vec![root_manifest];
    for member in members {
        let member = member
            .as_str()
            .unwrap_or_else(|| panic!("workspace member {member} is not a path string"));
        let manifest = root.join(member).join("Cargo.toml");
        assert!(
            manifest.is_file(),
            "workspace member {member} has no manifest at {}",
            manifest.display()
        );
        manifests.push(manifest);
    }
    manifests
}

/// The dependency tables of `manifest` that declare the banned crate,
/// including the ones nested under `[target.'cfg(…)']`.
fn banned_declarations(manifest: &Path) -> Vec<String> {
    let table = parse_manifest(manifest);
    let mut tables = Vec::new();
    for name in DEPENDENCY_TABLES {
        if declares_banned(table.get(*name)) {
            tables.push((*name).to_owned());
        }
    }
    let Some(targets) = table.get(TARGET_TABLE).and_then(toml::Value::as_table) else {
        return tables;
    };
    for (cfg, target) in targets {
        for name in DEPENDENCY_TABLES {
            if declares_banned(target.get(*name)) {
                tables.push(format!("{TARGET_TABLE}.'{cfg}'.{name}"));
            }
        }
    }
    tables
}

fn declares_banned(table: Option<&toml::Value>) -> bool {
    table
        .and_then(toml::Value::as_table)
        .is_some_and(|table| table.contains_key(BANNED_DEPENDENCY))
}

fn parse_manifest(path: &Path) -> toml::Table {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    text.parse::<toml::Table>()
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}
