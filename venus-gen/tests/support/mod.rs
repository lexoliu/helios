//! Shared by the integration tests: the verified pinned inputs.

use std::path::PathBuf;

use helios_venus_gen::inputs::{CACHE_DIR_ENV, Inputs, MANIFEST_PATH, Manifest};

/// The workspace root (the parent of this crate's directory).
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("venus-gen sits directly under the workspace root")
        .to_owned()
}

/// Opens the pinned inputs, failing the test with the typed error (which
/// names the fetch command) when they are missing or differ from the manifest.
pub fn inputs() -> Inputs {
    let root = workspace_root();
    let manifest = Manifest::read(&root.join(MANIFEST_PATH)).expect("manifest reads");
    let cache = Inputs::cache_dir(&root, std::env::var(CACHE_DIR_ENV).ok().as_deref());
    Inputs::open(manifest, &cache).unwrap_or_else(|error| panic!("{error}"))
}
