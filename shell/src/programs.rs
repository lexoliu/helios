use std::fs;
use std::path::Path;

use anyhow::{bail, Context as _, Result};
use helios_shell_protocol::system::programs::{self, LaunchRequest};
use wasmparser::Parser;
use wit_component::ComponentEncoder;

use crate::serial::RpcClient;

pub struct StartedProgram {
    pub instance_id: u64,
    pub name: String,
}

pub async fn run(client: &mut RpcClient, path: &str, args: &[String]) -> Result<StartedProgram> {
    let path = Path::new(path);
    let wasm = load_component(path)?;
    let name = infer_instance_name(path)?;
    let instance_id = programs::launch(
        &*client,
        &LaunchRequest {
            name: name.clone(),
            args: args.to_vec(),
            wasm,
        },
    )
    .await?;

    Ok(StartedProgram { instance_id, name })
}

fn load_component(path: &Path) -> Result<Vec<u8>> {
    let wasm = fs::read(path)
        .with_context(|| format!("failed to read local wasm file {}", path.display()))?;
    if Parser::is_component(&wasm) {
        return Ok(wasm);
    }

    ComponentEncoder::default()
        .module(&wasm)
        .with_context(|| format!("failed to load core wasm module {}", path.display()))?
        .validate(true)
        .encode()
        .with_context(|| format!("failed to encode component {}", path.display()))
}

fn infer_instance_name(path: &Path) -> Result<String> {
    let Some(name) = path.file_name() else {
        bail!("path {:?} does not name a runnable file", path);
    };
    let Some(name) = name.to_str() else {
        bail!("path {:?} contains a non-utf8 file name", path);
    };
    if name.is_empty() {
        bail!("path {:?} does not name a runnable file", path);
    }
    Ok(name.to_owned())
}
