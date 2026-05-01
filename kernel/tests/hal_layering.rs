use std::fs;
use std::path::{Path, PathBuf};

const BANNED_HAL_TOKENS: &[&str] = &[
    "wasm",
    "wasi",
    "wasix",
    "wasmtime",
    "cranelift",
    "wit-bindgen",
    "wit_bindgen",
    "wit/",
    "wit:",
    "`wit`",
];

#[test]
fn hal_does_not_name_wasm_or_wasi_abstractions() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .expect("kernel crate must live under the repository root");
    let mut files = Vec::new();
    collect_files(&repo_root.join("hal/src"), &mut files);
    files.push(repo_root.join("hal/Cargo.toml"));

    let mut leaks = Vec::new();
    for path in files {
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let lowered = content.to_ascii_lowercase();
        for token in BANNED_HAL_TOKENS {
            if lowered.contains(token) {
                leaks.push(format!("{} contains {token:?}", path.display()));
            }
        }
    }

    assert!(
        leaks.is_empty(),
        "HAL must stay hardware-only; adapter/runtime terms leaked into HAL:\n{}",
        leaks.join("\n")
    );
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
    let metadata = fs::metadata(path)
        .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", path.display()));
    if metadata.is_file() {
        if matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("rs" | "toml")
        ) {
            files.push(path.to_owned());
        }
        return;
    }

    for entry in fs::read_dir(path)
        .unwrap_or_else(|error| panic!("failed to read directory {}: {error}", path.display()))
    {
        let entry = entry
            .unwrap_or_else(|error| panic!("failed to read entry in {}: {error}", path.display()));
        collect_files(&entry.path(), files);
    }
}
