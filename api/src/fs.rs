//! Async filesystem helpers for component programs.
//!
//! The API surface intentionally stays small and value-oriented. Programs can
//! continue using synchronous `std::fs` when that is sufficient, and use these
//! helpers where non-blocking file I/O is required.

use std::path::{Component, Path};
use std::string::String;
use std::vec::Vec;

use crate::bindings::wasi::filesystem::preopens;
use crate::bindings::wasi::filesystem::types::{Descriptor, DescriptorFlags, OpenFlags, PathFlags};
use crate::error;
use crate::error::Result;
use futures_lite::future::zip;

const ROOT_PATH: &str = "/";

pub async fn read(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = path.as_ref();
    let display = display_path(path);
    let descriptor = open_file(path).await?;
    let (stream, result) = descriptor.read_via_stream(0);
    let bytes = stream.collect().await;
    result
        .await
        .map_err(|code| error::filesystem(&display, code))?;
    Ok(bytes)
}

pub async fn read_to_string(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    let display = display_path(path);
    let bytes = read(path).await?;
    String::from_utf8(bytes).map_err(|source| error::invalid_utf8(&display, source))
}

pub async fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<()> {
    let path = path.as_ref();
    let descriptor = open_file_for_write(path).await?;
    let (mut tx, rx) = crate::bindings::wit_stream::new();
    let bytes = contents.as_ref().to_vec();
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
    let mut components = normalized_components(path)?;
    let display = display_path(path);
    let file_name = components.pop().ok_or_else(|| {
        error::filesystem(
            &display,
            crate::bindings::wasi::filesystem::types::ErrorCode::Invalid,
        )
    })?;
    let mut descriptor = root_descriptor()?;

    for component in components {
        descriptor = descriptor
            .open_at(
                PathFlags::empty(),
                component,
                OpenFlags::DIRECTORY,
                DescriptorFlags::READ,
            )
            .await
            .map_err(|code| error::filesystem(&display, code))?;
    }

    descriptor
        .open_at(
            PathFlags::empty(),
            file_name,
            OpenFlags::empty(),
            DescriptorFlags::READ,
        )
        .await
        .map_err(|code| error::filesystem(&display, code))
}

async fn open_file_for_write(path: &Path) -> Result<Descriptor> {
    let mut components = normalized_components(path)?;
    let display = display_path(path);
    let file_name = components.pop().ok_or_else(|| {
        error::filesystem(
            &display,
            crate::bindings::wasi::filesystem::types::ErrorCode::Invalid,
        )
    })?;
    let mut descriptor = root_descriptor()?;

    for component in components {
        descriptor = descriptor
            .open_at(
                PathFlags::empty(),
                component,
                OpenFlags::DIRECTORY,
                DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
            )
            .await
            .map_err(|code| error::filesystem(&display, code))?;
    }

    descriptor
        .open_at(
            PathFlags::empty(),
            file_name,
            OpenFlags::CREATE | OpenFlags::TRUNCATE,
            DescriptorFlags::WRITE,
        )
        .await
        .map_err(|code| error::filesystem(&display, code))
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
