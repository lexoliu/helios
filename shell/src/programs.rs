use anyhow::{bail, Context as _, Result};
use helios_shell_protocol::debugger::filesystem;
use helios_shell_protocol::system::programs::{self, ExecRequest};
use wasmparser::Parser;
use wit_component::ComponentEncoder;

use crate::filesystem::normalize_path;
use crate::serial::RpcClient;

const PROGRAM_SEARCH_DIRECTORIES: &[&str] = &["/bin"];

pub struct StartedProgram {
    pub instance_id: u64,
    pub name: String,
}

pub async fn exec(client: &mut RpcClient, path: &str, args: &[String]) -> Result<StartedProgram> {
    let resolved = resolve_program(client, path).await?;
    let wasm = load_component(&resolved.path, &resolved.wasm)?;
    let name = infer_instance_name(&resolved.path)?;
    let instance_id = programs::exec(
        &*client,
        &ExecRequest {
            name: name.clone(),
            args: args.to_vec(),
            wasm,
        },
    )
    .await?;

    Ok(StartedProgram { instance_id, name })
}

struct ResolvedProgram {
    path: String,
    wasm: Vec<u8>,
}

async fn resolve_program(client: &mut RpcClient, input: &str) -> Result<ResolvedProgram> {
    let candidates = candidate_paths(input)?;
    let mut errors = Vec::new();

    for path in candidates {
        match filesystem::read(client, &path).await {
            Ok(wasm) => return Ok(ResolvedProgram { path, wasm }),
            Err(error) => errors.push(format!("{path}: {error:#}")),
        }
    }

    bail!(
        "failed to locate runnable program {input:?} in the guest filesystem:\n{}",
        errors.join("\n")
    );
}

fn load_component(path: &str, wasm: &[u8]) -> Result<Vec<u8>> {
    if Parser::is_component(&wasm) {
        return Ok(wasm.to_vec());
    }

    ComponentEncoder::default()
        .module(&wasm)
        .with_context(|| format!("failed to load core wasm module {path}"))?
        .validate(true)
        .encode()
        .with_context(|| format!("failed to encode component {path}"))
}

fn infer_instance_name(path: &str) -> Result<String> {
    let Some(name) = path.rsplit('/').next() else {
        bail!("path {path:?} does not name a runnable file");
    };
    if name.is_empty() {
        bail!("path {path:?} does not name a runnable file");
    }

    Ok(name.strip_suffix(".wasm").unwrap_or(name).to_owned())
}

fn candidate_paths(input: &str) -> Result<Vec<String>> {
    if input.contains('/') || input.starts_with('.') {
        return explicit_candidate_paths(input);
    }

    let mut candidates = Vec::with_capacity(PROGRAM_SEARCH_DIRECTORIES.len() * 2);
    for directory in PROGRAM_SEARCH_DIRECTORIES {
        candidates.push(format!("{directory}/{input}"));
        if !input.ends_with(".wasm") {
            candidates.push(format!("{directory}/{input}.wasm"));
        }
    }
    Ok(candidates)
}

fn explicit_candidate_paths(input: &str) -> Result<Vec<String>> {
    let normalized = normalize_path(input)?;
    if normalized.ends_with(".wasm") {
        return Ok(vec![normalized]);
    }

    Ok(vec![normalized.clone(), format!("{normalized}.wasm")])
}

#[cfg(test)]
mod tests {
    use super::{candidate_paths, infer_instance_name};

    #[test]
    fn bare_program_name_searches_bin_with_wasm_suffix() {
        assert_eq!(
            candidate_paths("ping").expect("bare command name must resolve"),
            vec!["/bin/ping", "/bin/ping.wasm"]
        );
    }

    #[test]
    fn explicit_paths_gain_wasm_suffix_when_missing() {
        assert_eq!(
            candidate_paths("/tools/ping").expect("explicit path must resolve"),
            vec!["/tools/ping", "/tools/ping.wasm"]
        );
    }

    #[test]
    fn instance_name_strips_wasm_extension() {
        assert_eq!(
            infer_instance_name("/bin/ping.wasm").expect("instance name must infer"),
            "ping"
        );
    }
}
