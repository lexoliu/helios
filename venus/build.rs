//! Generates `protocol.rs` from the pinned venus-protocol inputs.
//!
//! No network: `helios-cli venus-protocol-fetch` fills the cache directory,
//! and every file is verified against `protocol-inputs.toml` before it is read.

use std::env;
use std::fs;
use std::path::PathBuf;

use helios_venus_gen::analysis::Analysis;
use helios_venus_gen::emit;
use helios_venus_gen::inputs::{CACHE_DIR_ENV, Inputs, Manifest};
use helios_venus_gen::registry::Registry;

#[derive(Debug, thiserror::Error)]
enum BuildError {
    #[error(transparent)]
    Inputs(#[from] helios_venus_gen::inputs::InputsError),
    #[error(transparent)]
    Registry(#[from] helios_venus_gen::registry::RegistryError),
    #[error(transparent)]
    Emit(#[from] emit::EmitError),
    #[error("{what}: {source}")]
    Io {
        what: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cargo did not set {name}")]
    Env { name: &'static str },
}

fn var(name: &'static str) -> Result<String, BuildError> {
    env::var(name).map_err(|_| BuildError::Env { name })
}

fn run() -> Result<(), BuildError> {
    let package = PathBuf::from(var("CARGO_MANIFEST_DIR")?);
    let workspace_root = package
        .parent()
        .ok_or(BuildError::Env {
            name: "CARGO_MANIFEST_DIR",
        })?
        .to_owned();
    let manifest_path = package.join("protocol-inputs.toml");
    println!("cargo::rerun-if-changed={}", manifest_path.display());
    println!("cargo::rerun-if-env-changed={CACHE_DIR_ENV}");

    // The C compiler the parity test drives is configured for this host.
    println!(
        "cargo::rustc-env=HELIOS_VENUS_HOST_TARGET={}",
        var("TARGET")?
    );

    let manifest = Manifest::read(&manifest_path)?;
    let cache = Inputs::cache_dir(&workspace_root, env::var(CACHE_DIR_ENV).ok().as_deref());
    let inputs = Inputs::open(manifest, &cache)?;
    for file in &inputs.manifest().files {
        println!(
            "cargo::rerun-if-changed={}",
            inputs.path(&file.path).display()
        );
    }

    let vk = inputs.path("xmls/vk.xml");
    let serialization = inputs.path("xmls/VK_EXT_command_serialization.xml");
    let venus = inputs.path("xmls/VK_MESA_venus_protocol.xml");
    let registry = Registry::parse(&vk, &[&serialization, &venus])?;
    let analysis = Analysis::new(registry)?;
    let source = emit::protocol_source(&analysis)?;

    // The module the parity test compiles; the library never includes it.
    let parity = emit::parity_source(&analysis)?;

    let out_dir = PathBuf::from(var("OUT_DIR")?);
    for (name, text) in [("protocol.rs", source), ("parity.rs", parity)] {
        let out = out_dir.join(name);
        fs::write(&out, text).map_err(|source| BuildError::Io {
            what: format!("failed to write {}", out.display()),
            source,
        })?;
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        println!("cargo::error={error}");
        std::process::exit(1);
    }
}
