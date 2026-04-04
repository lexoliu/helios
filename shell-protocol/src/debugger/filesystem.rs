#[cfg(feature = "host")]
use crate::transport::Client;
#[cfg(feature = "host")]
use anyhow::{Context as _, Result};
#[cfg(feature = "guest")]
use anyhow::{Context as _, Result, bail};
#[cfg(feature = "host")]
use futures_io::{AsyncRead, AsyncWrite};
#[cfg(feature = "guest")]
use futures_lite::future::zip;
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WriteRequest {
    path: String,
    bytes: Vec<u8>,
    append: bool,
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

impl WriteRequest {
    #[cfg(feature = "host")]
    pub(crate) fn new(path: &str, bytes: &[u8], append: bool) -> Self {
        Self {
            path: path.to_owned(),
            bytes: bytes.to_vec(),
            append,
        }
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
pub async fn read<R, W>(client: &Client<R, W>, path: &str) -> Result<Vec<u8>>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let request = postcard::to_allocvec(&PathRequest::new(path))
        .context("failed to encode debugger filesystem.read request")?;
    let bytes = client
        .invoke_raw(FILESYSTEM_INSTANCE, "read", request)
        .await
        .context("failed to invoke debugger filesystem.read")?;
    postcard::from_bytes(&bytes).context("failed to decode debugger filesystem.read response")
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
pub async fn mkdir<R, W>(client: &Client<R, W>, path: &str) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let request = postcard::to_allocvec(&PathRequest::new(path))
        .context("failed to encode debugger filesystem.create-directory request")?;
    let bytes = client
        .invoke_raw(FILESYSTEM_INSTANCE, "create-directory", request)
        .await
        .context("failed to invoke debugger filesystem.create-directory")?;
    postcard::from_bytes::<()>(&bytes)
        .context("failed to decode debugger filesystem.create-directory response")
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

#[cfg(feature = "host")]
pub async fn write<R, W>(
    client: &Client<R, W>,
    path: &str,
    bytes: &[u8],
    append: bool,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let request = postcard::to_allocvec(&WriteRequest::new(path, bytes, append))
        .context("failed to encode debugger filesystem.write request")?;
    let bytes = client
        .invoke_raw(FILESYSTEM_INSTANCE, "write", request)
        .await
        .context("failed to invoke debugger filesystem.write")?;
    postcard::from_bytes::<()>(&bytes)
        .context("failed to decode debugger filesystem.write response")
}

#[cfg(feature = "guest")]
pub(crate) fn supports(instance: &str, func: &str) -> bool {
    matches!(
        (instance, func),
        (FILESYSTEM_INSTANCE, "list")
            | (FILESYSTEM_INSTANCE, "read")
            | (FILESYSTEM_INSTANCE, "remove")
            | (FILESYSTEM_INSTANCE, "create-directory")
            | (FILESYSTEM_INSTANCE, "touch")
            | (FILESYSTEM_INSTANCE, "write")
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
        "read" => {
            let path = decode_path_request(payload, "filesystem.read")?;
            postcard::to_allocvec(&read_file(&path).await?)
                .context("failed to encode debugger filesystem.read response")
        }
        "remove" => {
            let path = decode_path_request(payload, "filesystem.remove")?;
            remove_path(&path).await?;
            postcard::to_allocvec(&())
                .context("failed to encode debugger filesystem.remove response")
        }
        "create-directory" => {
            let path = decode_path_request(payload, "filesystem.create-directory")?;
            create_directory_path(&path).await?;
            postcard::to_allocvec(&())
                .context("failed to encode debugger filesystem.create-directory response")
        }
        "touch" => {
            let path = decode_path_request(payload, "filesystem.touch")?;
            touch_path(&path).await?;
            postcard::to_allocvec(&())
                .context("failed to encode debugger filesystem.touch response")
        }
        "write" => {
            let request = postcard::from_bytes::<WriteRequest>(payload)
                .context("failed to decode filesystem.write request payload")?;
            write_file(&request.path, &request.bytes, request.append).await?;
            postcard::to_allocvec(&())
                .context("failed to encode debugger filesystem.write response")
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
async fn read_file(path: &str) -> Result<Vec<u8>> {
    let descriptor = open_path(path, DescriptorFlags::READ, OpenFlags::empty()).await?;
    let descriptor_type = descriptor.get_type().await.map_err(|code| {
        anyhow::anyhow!("failed to inspect debugger filesystem path {path}: {code:?}")
    })?;
    if matches!(descriptor_type, DescriptorType::Directory) {
        bail!("debugger filesystem path {path} is a directory");
    }
    let (stream, result) = descriptor.read_via_stream(0);
    let bytes = stream.collect().await;
    result
        .await
        .map_err(|code| anyhow::anyhow!("failed to read debugger file {path}: {code:?}"))?;
    Ok(bytes)
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
async fn create_directory_path(path: &str) -> Result<()> {
    let (parent, name) = open_parent_directory(path).await?;
    parent
        .create_directory_at(name)
        .await
        .map_err(|code| anyhow::anyhow!("failed to create debugger directory {path}: {code:?}"))?;
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
async fn write_file(path: &str, bytes: &[u8], append: bool) -> Result<()> {
    let open_flags = if append {
        OpenFlags::CREATE
    } else {
        OpenFlags::CREATE | OpenFlags::TRUNCATE
    };
    let descriptor = open_path(path, DescriptorFlags::WRITE, open_flags).await?;
    let offset = if append {
        descriptor
            .stat()
            .await
            .map_err(|code| anyhow::anyhow!("failed to stat debugger file {path}: {code:?}"))?
            .size
    } else {
        0
    };
    let (mut tx, rx) = helios_api::bindings::wit_stream::new();
    let bytes = bytes.to_vec();
    let (write_result, feed_result) = zip(
        async move {
            descriptor
                .write_via_stream(rx, offset)
                .await
                .map_err(|code| anyhow::anyhow!("failed to write debugger file {path}: {code:?}"))
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
