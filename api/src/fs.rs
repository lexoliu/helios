//! Async filesystem helpers for component programs.
//!
//! The API surface intentionally stays small and value-oriented. Programs can
//! continue using synchronous `std::fs` when that is sufficient, and use these
//! helpers where non-blocking file I/O is required.

use std::path::{Component, Path};
use std::string::String;
use std::vec::Vec;

use crate::bindings::wasi::filesystem::preopens;
use crate::bindings::wasi::filesystem::types::{
    Descriptor, DescriptorFlags, DescriptorType, ErrorCode, OpenFlags, PathFlags,
};
use crate::error;
use crate::error::Result;
use futures_lite::future::zip;

const ROOT_PATH: &str = "/";
const FILE_READ_RESERVE_CHUNK: usize = 64 * 1024;

/// [`FileSystem`] backed by the program's WASI preopens, rooted at `/`.
#[derive(Clone, Copy, Debug, Default)]
pub struct WasiFileSystem;

/// Async filesystem operations abstracted from the WASI backend, so
/// program logic can be exercised against a test double.
///
/// Paths are absolute, `..` traversal is rejected, and every failure is
/// a `std::io::Error` naming the offending path.
pub trait FileSystem {
    fn read(&self, path: &Path) -> impl core::future::Future<Output = Result<Vec<u8>>> + Send;
    fn read_to_string(
        &self,
        path: &Path,
    ) -> impl core::future::Future<Output = Result<String>> + Send;
    fn write(
        &self,
        path: &Path,
        contents: &[u8],
    ) -> impl core::future::Future<Output = Result<()>> + Send;
    fn exists(&self, path: &Path) -> impl core::future::Future<Output = bool> + Send;
    fn is_file(&self, path: &Path) -> impl core::future::Future<Output = bool> + Send;
    fn is_dir(&self, path: &Path) -> impl core::future::Future<Output = bool> + Send;
}

/// The WASI-backed [`FileSystem`] value; free functions below are
/// shorthands for its methods.
pub const fn wasi_filesystem() -> WasiFileSystem {
    WasiFileSystem
}

/// Reads the entire file at `path`.
pub async fn read(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    read_impl(path.as_ref()).await
}

/// Reads the entire file at `path` as UTF-8.
pub async fn read_to_string(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    let display = display_path(path);
    let bytes = read_impl(path).await?;
    String::from_utf8(bytes).map_err(|source| error::invalid_utf8(&display, source))
}

/// Creates or truncates the file at `path` and writes `contents`.
pub async fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<()> {
    write_impl(path.as_ref(), contents.as_ref()).await
}

/// Whether `path` names an existing file or directory.
pub async fn exists(path: impl AsRef<Path>) -> bool {
    metadata_type(path).await.is_ok()
}

/// Whether `path` names an existing regular file.
pub async fn is_file(path: impl AsRef<Path>) -> bool {
    matches!(metadata_type(path).await, Ok(DescriptorType::RegularFile))
}

/// Whether `path` names an existing directory.
pub async fn is_dir(path: impl AsRef<Path>) -> bool {
    matches!(metadata_type(path).await, Ok(DescriptorType::Directory))
}

impl FileSystem for WasiFileSystem {
    fn read(&self, path: &Path) -> impl core::future::Future<Output = Result<Vec<u8>>> + Send {
        read_impl(path)
    }

    fn read_to_string(
        &self,
        path: &Path,
    ) -> impl core::future::Future<Output = Result<String>> + Send {
        let path = path.to_owned();
        async move {
            let display = display_path(&path);
            let bytes = read_impl(&path).await?;
            String::from_utf8(bytes).map_err(|source| error::invalid_utf8(&display, source))
        }
    }

    fn write(
        &self,
        path: &Path,
        contents: &[u8],
    ) -> impl core::future::Future<Output = Result<()>> + Send {
        let path = path.to_owned();
        let contents = contents.to_vec();
        async move { write_impl(&path, &contents).await }
    }

    fn exists(&self, path: &Path) -> impl core::future::Future<Output = bool> + Send {
        let path = path.to_owned();
        async move { metadata_type(path).await.is_ok() }
    }

    fn is_file(&self, path: &Path) -> impl core::future::Future<Output = bool> + Send {
        let path = path.to_owned();
        async move { matches!(metadata_type(path).await, Ok(DescriptorType::RegularFile)) }
    }

    fn is_dir(&self, path: &Path) -> impl core::future::Future<Output = bool> + Send {
        let path = path.to_owned();
        async move { matches!(metadata_type(path).await, Ok(DescriptorType::Directory)) }
    }
}

async fn read_impl(path: &Path) -> Result<Vec<u8>> {
    let display = display_path(path);
    let descriptor = open_file(path).await?;
    let expected_size = descriptor
        .stat()
        .await
        .map_err(|code| error::filesystem(&display, code))?
        .size;
    let initial_capacity = usize::try_from(expected_size)
        .map_err(|_| error::file_too_large(&display, expected_size))?;
    let (stream, result) = descriptor.read_via_stream(0);
    let mut stream = stream;
    let mut bytes = Vec::with_capacity(initial_capacity.saturating_add(1));
    loop {
        if bytes.len() == bytes.capacity() {
            bytes.reserve(FILE_READ_RESERVE_CHUNK);
        }
        let (status, next) = stream.read(bytes).await;
        bytes = next;
        match status {
            crate::wit_bindgen::StreamResult::Complete(_) => {}
            crate::wit_bindgen::StreamResult::Dropped => break,
            crate::wit_bindgen::StreamResult::Cancelled => unreachable!(),
        }
    }
    result
        .await
        .map_err(|code| error::filesystem(&display, code))?;
    Ok(bytes)
}

async fn write_impl(path: &Path, contents: &[u8]) -> Result<()> {
    let descriptor = open_file_for_write(path).await?;
    let (mut tx, rx) = crate::bindings::wit_stream::new();
    let bytes = contents.to_vec();
    let display = display_path(path);
    let (write_result, feed_result) = zip(
        async move {
            descriptor
                .write_via_stream(rx, 0)
                .await
                .map_err(|code| error::filesystem(&display, code))
        },
        async move {
            tx.write(bytes).await;
            drop(tx);
            Ok::<(), std::io::Error>(())
        },
    )
    .await;
    feed_result?;
    write_result
}

fn root_descriptor() -> Result<Descriptor> {
    preopens::get_directories()
        .into_iter()
        .find_map(|(descriptor, path)| (path == ROOT_PATH).then_some(descriptor))
        .ok_or_else(error::missing_root_directory)
}

async fn open_file(path: &Path) -> Result<Descriptor> {
    open_leaf(path, OpenFlags::empty(), DescriptorFlags::READ).await
}

async fn open_directory(path: &Path, writable: bool) -> Result<Descriptor> {
    if path == Path::new(ROOT_PATH) {
        return root_descriptor();
    }

    let flags = if writable {
        DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY
    } else {
        DescriptorFlags::READ
    };
    open_leaf(path, OpenFlags::DIRECTORY, flags).await
}

async fn open_file_for_write(path: &Path) -> Result<Descriptor> {
    let mut components = normalized_components(path)?;
    let display = display_path(path);
    let file_name = components
        .pop()
        .ok_or_else(|| error::filesystem(&display, ErrorCode::Invalid))?;
    let parent = open_parent_directory(&components, true, &display).await?;

    parent
        .open_at(
            PathFlags::empty(),
            file_name,
            OpenFlags::CREATE | OpenFlags::TRUNCATE,
            DescriptorFlags::WRITE,
        )
        .await
        .map_err(|code| error::filesystem(&display, code))
}

async fn open_leaf(
    path: &Path,
    open_flags: OpenFlags,
    flags: DescriptorFlags,
) -> Result<Descriptor> {
    let mut components = normalized_components(path)?;
    let display = display_path(path);
    let file_name = components
        .pop()
        .ok_or_else(|| error::filesystem(&display, ErrorCode::Invalid))?;
    let parent = open_parent_directory(
        &components,
        flags == (DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY),
        &display,
    )
    .await?;

    parent
        .open_at(PathFlags::empty(), file_name, open_flags, flags)
        .await
        .map_err(|code| error::filesystem(&display, code))
}

async fn open_parent_directory(
    components: &[String],
    writable: bool,
    display: &str,
) -> Result<Descriptor> {
    let mut descriptor = root_descriptor()?;
    let flags = if writable {
        DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY
    } else {
        DescriptorFlags::READ
    };

    for component in components {
        descriptor = descriptor
            .open_at(
                PathFlags::empty(),
                component.clone(),
                OpenFlags::DIRECTORY,
                flags,
            )
            .await
            .map_err(|code| error::filesystem(display, code))?;
    }

    Ok(descriptor)
}

async fn metadata_type(path: impl AsRef<Path>) -> Result<DescriptorType> {
    let path = path.as_ref();
    if path == Path::new(ROOT_PATH) {
        return Ok(DescriptorType::Directory);
    }

    match open_file(path).await {
        Ok(_) => Ok(DescriptorType::RegularFile),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
            ) =>
        {
            open_directory(path, false).await?;
            Ok(DescriptorType::Directory)
        }
        Err(error) => Err(error),
    }
}

fn normalized_components(path: &Path) -> Result<Vec<String>> {
    let mut components = Vec::new();
    let display = display_path(path);

    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return Err(error::parent_traversal(&display)),
            Component::Normal(segment) => {
                let segment = segment
                    .to_str()
                    .ok_or_else(|| error::non_utf8_path(&display))?;
                components.push(segment.to_owned());
            }
            Component::Prefix(_) => return Err(error::non_utf8_path(&display)),
        }
    }

    Ok(components)
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}
