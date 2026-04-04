use std::env;
use std::fs;
use std::path::PathBuf;

use proc_macro::TokenStream;
use proc_macro2::Literal;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{LitStr, Result, parse_macro_input};
use walkdir::WalkDir;

struct WasmMacroInput {
    wasm_path: LitStr,
}

impl Parse for WasmMacroInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let wasm_path = input.parse()?;
        Ok(Self { wasm_path })
    }
}

struct BootFsMacroInput {
    root: LitStr,
}

impl Parse for BootFsMacroInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
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

fn expand_embed_wasm(input: WasmMacroInput) -> std::result::Result<TokenStream, String> {
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

fn expand_bootfs(input: BootFsMacroInput) -> std::result::Result<TokenStream, String> {
    let root = resolve_input_path(&input.root)?;
    let root = fs::canonicalize(&root)
        .map_err(|error| format!("failed to resolve {}: {error}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(&root).sort_by_file_name() {
        let entry = entry.map_err(|error| format!("failed to walk {}: {error}", root.display()))?;
        if !entry.file_type().is_file() {
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
        let contents = fs::read(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        files.push((
            LitStr::new(&relative, input.root.span()),
            Literal::byte_string(&contents),
        ));
    }

    let entries = files.iter().map(|(path, contents)| {
        quote! {
            ::helios_kernel::EmbeddedBootFile::new(#path, #contents)
        }
    });

    Ok(quote! {
        ::helios_kernel::EmbeddedBootFs::new(&[
            #(#entries),*
        ])
    }
    .into())
}

fn resolve_input_path(path: &LitStr) -> std::result::Result<PathBuf, String> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map_err(|error| format!("CARGO_MANIFEST_DIR is missing: {error}"))?;
    Ok(PathBuf::from(manifest_dir).join(path.value()))
}
