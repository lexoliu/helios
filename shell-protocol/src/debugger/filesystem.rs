#[cfg(feature = "host")]
use crate::transport::Client;
#[cfg(feature = "host")]
use anyhow::{Context as _, Result};
#[cfg(feature = "guest")]
use anyhow::{Context as _, Result, bail};
#[cfg(feature = "host")]
use futures_io::{AsyncRead, AsyncWrite};
#[cfg(feature = "guest")]
use helios_api::bindings::wasi::filesystem::preopens;
#[cfg(feature = "guest")]
use helios_api::bindings::wasi::filesystem::types::{
    Descriptor, DescriptorFlags, DescriptorType, DirectoryEntry as WasiDirectoryEntry, OpenFlags,
    PathFlags,
};
use serde::{Deserialize, Serialize};
#[cfg(feature = "guest")]
use std::path::{Component, Path};

const FILESYSTEM_INSTANCE: &str = "helios:debugger/filesystem@0.1.0";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: EntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EntryKind {
    Directory,
    File,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PathRequest {
    path: String,
}

impl PathRequest {
    #[cfg(feature = "host")]
    pub(crate) fn new(path: &str) -> Self {
        Self {
            path: path.to_owned(),
        }
    }

    #[cfg(feature = "guest")]
    pub(crate) fn into_path(self) -> String {
        self.path
    }
}

#[cfg(feature = "host")]
pub async fn list<R, W>(client: &Client<R, W>, path: &str) -> Result<Vec<DirectoryEntry>>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let request = postcard::to_allocvec(&PathRequest::new(path))
        .context("failed to encode debugger filesystem.list request")?;
    let bytes = client
        .invoke_raw(FILESYSTEM_INSTANCE, "list", request)
        .await
        .context("failed to invoke debugger filesystem.list")?;
    postcard::from_bytes(&bytes).context("failed to decode debugger filesystem.list response")
}

#[cfg(feature = "host")]
pub async fn remove<R, W>(client: &Client<R, W>, path: &str) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let request = postcard::to_allocvec(&PathRequest::new(path))
        .context("failed to encode debugger filesystem.remove request")?;
    let bytes = client
        .invoke_raw(FILESYSTEM_INSTANCE, "remove", request)
        .await
        .context("failed to invoke debugger filesystem.remove")?;
    postcard::from_bytes::<()>(&bytes)
        .context("failed to decode debugger filesystem.remove response")
}

#[cfg(feature = "host")]
pub async fn touch<R, W>(client: &Client<R, W>, path: &str) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let request = postcard::to_allocvec(&PathRequest::new(path))
        .context("failed to encode debugger filesystem.touch request")?;
    let bytes = client
        .invoke_raw(FILESYSTEM_INSTANCE, "touch", request)
        .await
        .context("failed to invoke debugger filesystem.touch")?;
    postcard::from_bytes::<()>(&bytes)
        .context("failed to decode debugger filesystem.touch response")
}

#[cfg(feature = "guest")]
pub(crate) fn supports(instance: &str, func: &str) -> bool {
    matches!(
        (instance, func),
        (FILESYSTEM_INSTANCE, "list")
            | (FILESYSTEM_INSTANCE, "remove")
            | (FILESYSTEM_INSTANCE, "touch")
    )
}

#[cfg(feature = "guest")]
pub(crate) async fn dispatch(func: &str, payload: &[u8]) -> Result<Vec<u8>> {
    match func {
        "list" => {
            let path = decode_path_request(payload, "filesystem.list")?;
            postcard::to_allocvec(&list_directory(&path).await?)
                .context("failed to encode debugger filesystem.list response")
        }
        "remove" => {
            let path = decode_path_request(payload, "filesystem.remove")?;
            remove_path(&path).await?;
            postcard::to_allocvec(&())
                .context("failed to encode debugger filesystem.remove response")
        }
        "touch" => {
            let path = decode_path_request(payload, "filesystem.touch")?;
            touch_path(&path).await?;
            postcard::to_allocvec(&())
                .context("failed to encode debugger filesystem.touch response")
        }
        _ => unreachable!("filesystem::supports must filter unsupported debugger methods"),
    }
}

#[cfg(feature = "guest")]
fn decode_path_request(payload: &[u8], operation: &str) -> Result<String> {
    let request = postcard::from_bytes::<PathRequest>(payload)
        .with_context(|| format!("failed to decode {operation} request payload"))?;
    Ok(request.into_path())
}

#[cfg(feature = "guest")]
async fn list_directory(path: &str) -> Result<Vec<DirectoryEntry>> {
    let descriptor = open_path(path, DescriptorFlags::READ, OpenFlags::empty()).await?;
    let descriptor_type = descriptor.get_type().await.map_err(|code| {
        anyhow::anyhow!("failed to inspect debugger filesystem path {path}: {code:?}")
    })?;
    if matches!(descriptor_type, DescriptorType::Directory) {
        let (stream, result) = descriptor.read_directory();
        let mut entries = stream
            .collect()
            .await
            .into_iter()
            .map(convert_directory_entry)
            .collect::<Vec<_>>();
        result.await.map_err(|code| {
            anyhow::anyhow!("failed to read debugger directory {path}: {code:?}")
        })?;
        entries.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.name.cmp(&right.name))
        });
        return Ok(entries);
    }

    Ok(vec![DirectoryEntry {
        name: display_name(path),
        kind: convert_entry_kind(descriptor_type),
    }])
}

#[cfg(feature = "guest")]
async fn remove_path(path: &str) -> Result<()> {
    let (parent, name) = open_parent_directory(path).await?;
    let stat = parent
        .stat_at(PathFlags::empty(), name.clone())
        .await
        .map_err(|code| {
            anyhow::anyhow!("failed to inspect debugger filesystem path {path}: {code:?}")
        })?;
    match stat.type_ {
        DescriptorType::Directory => parent.remove_directory_at(name).await.map_err(|code| {
            anyhow::anyhow!("failed to remove debugger directory {path}: {code:?}")
        })?,
        _ => parent
            .unlink_file_at(name)
            .await
            .map_err(|code| anyhow::anyhow!("failed to remove debugger file {path}: {code:?}"))?,
    }
    Ok(())
}

#[cfg(feature = "guest")]
async fn touch_path(path: &str) -> Result<()> {
    let (parent, name) = open_parent_directory(path).await?;
    let _ = parent
        .open_at(
            PathFlags::empty(),
            name,
            OpenFlags::CREATE,
            DescriptorFlags::WRITE,
        )
        .await
        .map_err(|code| anyhow::anyhow!("failed to touch debugger path {path}: {code:?}"))?;
    Ok(())
}

#[cfg(feature = "guest")]
fn root_descriptor() -> Result<Descriptor> {
    preopens::get_directories()
        .into_iter()
        .find_map(|(descriptor, path)| (path == "/").then_some(descriptor))
        .ok_or_else(|| anyhow::anyhow!("debugger is missing a preopened root directory"))
}

#[cfg(feature = "guest")]
async fn open_path(
    path: &str,
    flags: DescriptorFlags,
    open_flags: OpenFlags,
) -> Result<Descriptor> {
    let components = normalized_components(path)?;
    let mut descriptor = root_descriptor()?;

    for (index, component) in components.iter().enumerate() {
        let is_last = index + 1 == components.len();
        let next_open_flags = if is_last {
            open_flags
        } else {
            OpenFlags::DIRECTORY
        };
        let next_flags = if is_last {
            flags
        } else {
            DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY
        };
        descriptor = descriptor
            .open_at(
                PathFlags::empty(),
                component.clone(),
                next_open_flags,
                next_flags,
            )
            .await
            .map_err(|code| {
                anyhow::anyhow!("failed to open debugger filesystem path {path}: {code:?}")
            })?;
    }

    Ok(descriptor)
}

#[cfg(feature = "guest")]
async fn open_parent_directory(path: &str) -> Result<(Descriptor, String)> {
    let mut components = normalized_components(path)?;
    let name = components
        .pop()
        .ok_or_else(|| anyhow::anyhow!("path {path:?} does not refer to a removable entry"))?;
    let mut descriptor = root_descriptor()?;

    for component in components {
        descriptor = descriptor
            .open_at(
                PathFlags::empty(),
                component.clone(),
                OpenFlags::DIRECTORY,
                DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
            )
            .await
            .map_err(|code| {
                anyhow::anyhow!("failed to open parent directory for {path}: {code:?}")
            })?;
    }

    Ok((descriptor, name))
}

#[cfg(feature = "guest")]
fn normalized_components(path: &str) -> Result<Vec<String>> {
    let mut components = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(segment) => components.push(
                segment
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("path {path:?} contains a non-utf8 segment"))?
                    .to_owned(),
            ),
            Component::ParentDir => bail!("path {path:?} contains unsupported parent traversal"),
            Component::Prefix(_) => bail!("path {path:?} uses an unsupported path prefix"),
        }
    }
    Ok(components)
}

#[cfg(feature = "guest")]
fn convert_directory_entry(entry: WasiDirectoryEntry) -> DirectoryEntry {
    DirectoryEntry {
        name: entry.name,
        kind: convert_entry_kind(entry.type_),
    }
}

#[cfg(feature = "guest")]
fn convert_entry_kind(kind: DescriptorType) -> EntryKind {
    match kind {
        DescriptorType::Directory => EntryKind::Directory,
        DescriptorType::RegularFile => EntryKind::File,
        _ => EntryKind::Other,
    }
}

#[cfg(feature = "guest")]
fn display_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .unwrap_or_else(|| Path::new(path).as_os_str())
        .to_string_lossy()
        .into_owned()
}
