use std::path::Path;

use anyhow::{Context as _, Result, bail};

pub fn candidate_program_paths(input: &str, search_directories: &[&str]) -> Vec<String> {
    if input.contains('/') || input.starts_with('.') {
        return explicit_program_paths(input);
    }

    let mut candidates = Vec::with_capacity(search_directories.len() * 2);
    for directory in search_directories {
        candidates.push(format!("{directory}/{input}"));
        if !input.ends_with(".wasm") {
            candidates.push(format!("{directory}/{input}.wasm"));
        }
    }
    candidates
}

pub fn explicit_program_paths(input: &str) -> Vec<String> {
    if input.ends_with(".wasm") {
        return vec![input.to_owned()];
    }

    vec![input.to_owned(), format!("{input}.wasm")]
}

pub fn infer_program_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("program path does not end with a valid utf-8 file name")?;
    if name.is_empty() {
        bail!("program path does not name an executable")
    }
    Ok(name.strip_suffix(".wasm").unwrap_or(name).to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{candidate_program_paths, explicit_program_paths, infer_program_name};

    #[test]
    fn bare_program_name_searches_bin_with_wasm_suffix() {
        assert_eq!(
            candidate_program_paths("ping", &["/bin"]),
            vec!["/bin/ping", "/bin/ping.wasm"]
        );
    }

    #[test]
    fn explicit_paths_gain_wasm_suffix_when_missing() {
        assert_eq!(
            explicit_program_paths("/tools/ping"),
            vec!["/tools/ping", "/tools/ping.wasm"]
        );
    }

    #[test]
    fn instance_name_strips_wasm_extension() {
        assert_eq!(
            infer_program_name(Path::new("/bin/ping.wasm")).expect("instance name must infer"),
            "ping"
        );
    }
}
