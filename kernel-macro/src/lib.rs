use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use proc_macro::TokenStream;
use proc_macro2::Literal;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{LitStr, parse_macro_input};
use walkdir::WalkDir;

struct WasmMacroInput {
    wasm_path: LitStr,
}

impl Parse for WasmMacroInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let wasm_path = input.parse()?;
        Ok(Self { wasm_path })
    }
}

struct BootFsMacroInput {
    root: LitStr,
}

impl Parse for BootFsMacroInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        Ok(Self {
            root: input.parse()?,
        })
    }
}

#[proc_macro]
pub fn embed_wasm(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as WasmMacroInput);
    expand_embed_wasm(input)
        .unwrap_or_else(|error| panic!("failed to embed wasm for JIT loading: {error}"))
}

#[proc_macro]
pub fn bootfs(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as BootFsMacroInput);
    expand_bootfs(input).unwrap_or_else(|error| panic!("failed to embed bootfs directory: {error}"))
}

fn expand_embed_wasm(input: WasmMacroInput) -> Result<TokenStream, String> {
    let wasm_path = resolve_input_path(&input.wasm_path)?;
    let wasm_path = fs::canonicalize(&wasm_path)
        .map_err(|error| format!("failed to resolve {}: {error}", wasm_path.display()))?;
    let wasm = wat::parse_file(&wasm_path)
        .map_err(|error| format!("failed to parse {}: {error}", wasm_path.display()))?;
    let span = input.wasm_path.span();

    let name = wasm_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has no valid UTF-8 file name", wasm_path.display()))?;
    let name = LitStr::new(name, span);
    let bytes = Literal::byte_string(&wasm);

    Ok(quote! {
        ::helios_kernel::EmbeddedProgram::new(#name, #bytes)
    }
    .into())
}

fn expand_bootfs(input: BootFsMacroInput) -> Result<TokenStream, String> {
    let root = resolve_input_path(&input.root)?;
    let root = fs::canonicalize(&root)
        .map_err(|error| format!("failed to resolve {}: {error}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }

    let mut directories = Vec::new();
    let mut files = Vec::new();
    for entry in WalkDir::new(&root).sort_by_file_name() {
        let entry = entry.map_err(|error| format!("failed to walk {}: {error}", root.display()))?;
        if entry.path() == root {
            continue;
        }
        let path = entry.into_path();
        let relative = path.strip_prefix(&root).map_err(|error| {
            format!(
                "failed to strip root {} from {}: {error}",
                root.display(),
                path.display()
            )
        })?;
        let relative = relative
            .to_str()
            .ok_or_else(|| format!("{} has non-UTF8 relative path", path.display()))?
            .replace('\\', "/");
        let modified_nanos = file_modified_nanos(&path)?;
        if path.is_dir() {
            if !is_empty_directory(&path)? {
                continue;
            }
            directories.push((LitStr::new(&relative, input.root.span()), modified_nanos));
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let contents = fs::read(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        files.push((
            LitStr::new(&relative, input.root.span()),
            Literal::byte_string(&contents),
            modified_nanos,
        ));
    }

    let directories = directories.iter().map(|(path, modified_nanos)| {
        quote! {
            ::helios_kernel::EmbeddedBootDirectory::new(#path, #modified_nanos)
        }
    });
    let entries = files.iter().map(|(path, contents, modified_nanos)| {
        quote! {
            ::helios_kernel::EmbeddedBootFile::new(#path, #contents, #modified_nanos)
        }
    });

    Ok(quote! {
        ::helios_kernel::EmbeddedBootFs::new(&[
            #(#directories),*
        ], &[
            #(#entries),*
        ])
    }
    .into())
}

fn resolve_input_path(path: &LitStr) -> Result<PathBuf, String> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map_err(|error| format!("CARGO_MANIFEST_DIR is missing: {error}"))?;
    Ok(PathBuf::from(manifest_dir).join(path.value()))
}

fn file_modified_nanos(path: &std::path::Path) -> Result<u64, String> {
    let modified = fs::metadata(path)
        .map_err(|error| format!("failed to read metadata for {}: {error}", path.display()))?
        .modified()
        .map_err(|error| {
            format!(
                "failed to read modification time for {}: {error}",
                path.display()
            )
        })?;
    let duration = modified.duration_since(UNIX_EPOCH).map_err(|error| {
        format!(
            "modification time for {} is before the Unix epoch: {error}",
            path.display()
        )
    })?;
    duration
        .as_secs()
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(u64::from(duration.subsec_nanos())))
        .ok_or_else(|| format!("modification time for {} overflowed u64", path.display()))
}

fn is_empty_directory(path: &std::path::Path) -> Result<bool, String> {
    let mut entries = fs::read_dir(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    Ok(entries.next().is_none())
}
