use anyhow::{bail, Result};
use helios_shell_protocol::debugger::programs as debugger_programs;
use helios_shell_protocol::system::programs::ExecResult;
use shell_core::paths::normalize_absolute as normalize_path;
use shell_core::programs::candidate_program_paths;

use crate::serial::RpcClient;

const PROGRAM_SEARCH_DIRECTORIES: &[&str] = &["/bin"];

pub async fn exec(client: &mut RpcClient, path: &str, args: &[String]) -> Result<ExecResult> {
    let mut errors = Vec::new();

    for candidate in candidate_paths(path)? {
        match debugger_programs::exec_path(&*client, &candidate, args).await {
            Ok(result) => return Ok(result),
            Err(error) => errors.push(format!("{candidate}: {error:#}")),
        }
    }

    bail!(
        "failed to locate runnable program {path:?} in the guest filesystem:\n{}",
        errors.join("\n")
    )
}

fn candidate_paths(input: &str) -> Result<Vec<String>> {
    if input.contains('/') || input.starts_with('.') {
        return explicit_candidate_paths(input);
    }
    Ok(candidate_program_paths(input, PROGRAM_SEARCH_DIRECTORIES))
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
    use super::candidate_paths;

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

}
