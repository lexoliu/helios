use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PathPairRequest {
    source: String,
    destination: String,
}

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::debugger::methods::FILESYSTEM_INSTANCE;
    use crate::error::{RpcError, TransportError};
    use crate::transport::Client;
    use futures_io::{AsyncRead, AsyncWrite};

    impl PathRequest {
        pub(crate) fn new(path: &str) -> Self {
            Self {
                path: path.to_owned(),
            }
        }
    }

    impl WriteRequest {
        pub(crate) fn new(path: &str, bytes: &[u8], append: bool) -> Self {
            Self {
                path: path.to_owned(),
                bytes: bytes.to_vec(),
                append,
            }
        }
    }

    impl PathPairRequest {
        pub(crate) fn new(source: &str, destination: &str) -> Self {
            Self {
                source: source.to_owned(),
                destination: destination.to_owned(),
            }
        }
    }

    pub async fn list<R, W>(
        client: &Client<R, W>,
        path: &str,
    ) -> Result<Vec<DirectoryEntry>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = invoke(client, "list", &PathRequest::new(path)).await?;
        decode_response("list", &bytes)
    }

    pub async fn read<R, W>(client: &Client<R, W>, path: &str) -> Result<Vec<u8>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = invoke(client, "read", &PathRequest::new(path)).await?;
        decode_response("read", &bytes)
    }

    pub async fn remove<R, W>(client: &Client<R, W>, path: &str) -> Result<(), RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = invoke(client, "remove", &PathRequest::new(path)).await?;
        decode_response::<()>("remove", &bytes)
    }

    pub async fn mkdir<R, W>(client: &Client<R, W>, path: &str) -> Result<(), RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = invoke(client, "create-directory", &PathRequest::new(path)).await?;
        decode_response::<()>("create-directory", &bytes)
    }

    pub async fn touch<R, W>(client: &Client<R, W>, path: &str) -> Result<(), RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = invoke(client, "touch", &PathRequest::new(path)).await?;
        decode_response::<()>("touch", &bytes)
    }

    pub async fn write<R, W>(
        client: &Client<R, W>,
        path: &str,
        bytes: &[u8],
        append: bool,
    ) -> Result<(), RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let response = invoke(client, "write", &WriteRequest::new(path, bytes, append)).await?;
        decode_response::<()>("write", &response)
    }

    pub async fn copy<R, W>(
        client: &Client<R, W>,
        source: &str,
        destination: &str,
    ) -> Result<(), RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let response = invoke(client, "copy", &PathPairRequest::new(source, destination)).await?;
        decode_response::<()>("copy", &response)
    }

    pub async fn move_path<R, W>(
        client: &Client<R, W>,
        source: &str,
        destination: &str,
    ) -> Result<(), RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let response = invoke(client, "move", &PathPairRequest::new(source, destination)).await?;
        decode_response::<()>("move", &response)
    }

    async fn invoke<R, W, Req>(
        client: &Client<R, W>,
        func: &'static str,
        request: &Req,
    ) -> Result<Vec<u8>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
        Req: serde::Serialize,
    {
        let payload = postcard::to_allocvec(request).map_err(|source| RpcError::Encode {
            instance: FILESYSTEM_INSTANCE,
            func,
            source,
        })?;
        client
            .invoke_raw(FILESYSTEM_INSTANCE, func, payload)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: FILESYSTEM_INSTANCE,
                func,
                source,
            })
    }

    fn decode_response<T>(func: &'static str, bytes: &[u8]) -> Result<T, RpcError>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        postcard::from_bytes::<T>(bytes).map_err(|source| RpcError::Decode {
            instance: FILESYSTEM_INSTANCE,
            func,
            source,
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;

#[cfg(feature = "guest")]
mod guest {
    use super::*;
    use crate::debugger::methods::FILESYSTEM_INSTANCE;
    use crate::error::DispatchError;
    use futures_lite::future::zip;
    use helios_api::bindings::wasi::filesystem::preopens;
    use helios_api::bindings::wasi::filesystem::types::{
        Descriptor, DescriptorFlags, DescriptorType, DirectoryEntry as WasiDirectoryEntry,
        ErrorCode, OpenFlags, PathFlags,
    };
    use std::path::Path;

    impl PathRequest {
        pub(crate) fn into_path(self) -> String {
            self.path
        }
    }

    pub(crate) fn supports(instance: &str, func: &str) -> bool {
        matches!(
            (instance, func),
            (FILESYSTEM_INSTANCE, "list")
                | (FILESYSTEM_INSTANCE, "read")
                | (FILESYSTEM_INSTANCE, "remove")
                | (FILESYSTEM_INSTANCE, "create-directory")
                | (FILESYSTEM_INSTANCE, "touch")
                | (FILESYSTEM_INSTANCE, "write")
                | (FILESYSTEM_INSTANCE, "copy")
                | (FILESYSTEM_INSTANCE, "move")
        )
    }

    pub(crate) async fn dispatch(func: &str, payload: &[u8]) -> Result<Vec<u8>, DispatchError> {
        match func {
            "list" => {
                let path = decode_path_request(payload, "filesystem.list")?;
                encode_response("filesystem.list", &list_directory(&path).await?)
            }
            "read" => {
                let path = decode_path_request(payload, "filesystem.read")?;
                encode_response("filesystem.read", &read_file(&path).await?)
            }
            "remove" => {
                let path = decode_path_request(payload, "filesystem.remove")?;
                remove_path(&path).await?;
                encode_response("filesystem.remove", &())
            }
            "create-directory" => {
                let path = decode_path_request(payload, "filesystem.create-directory")?;
                create_directory_path(&path).await?;
                encode_response("filesystem.create-directory", &())
            }
            "touch" => {
                let path = decode_path_request(payload, "filesystem.touch")?;
                touch_path(&path).await?;
                encode_response("filesystem.touch", &())
            }
            "write" => {
                let request = postcard::from_bytes::<WriteRequest>(payload).map_err(|source| {
                    DispatchError::Decode {
                        operation: "filesystem.write",
                        source,
                    }
                })?;
                write_file(&request.path, &request.bytes, request.append).await?;
                encode_response("filesystem.write", &())
            }
            "copy" => {
                let request = decode_path_pair_request(payload, "filesystem.copy")?;
                copy_path(&request.source, &request.destination).await?;
                encode_response("filesystem.copy", &())
            }
            "move" => {
                let request = decode_path_pair_request(payload, "filesystem.move")?;
                move_guest_path(&request.source, &request.destination).await?;
                encode_response("filesystem.move", &())
            }
            _ => {
                unreachable!("filesystem::supports must filter unsupported debugger methods")
            }
        }
    }

    fn encode_response<T: serde::Serialize>(
        operation: &'static str,
        value: &T,
    ) -> Result<Vec<u8>, DispatchError> {
        postcard::to_allocvec(value).map_err(|source| DispatchError::Encode { operation, source })
    }

    fn decode_path_request(
        payload: &[u8],
        operation: &'static str,
    ) -> Result<String, DispatchError> {
        let request = postcard::from_bytes::<PathRequest>(payload)
            .map_err(|source| DispatchError::Decode { operation, source })?;
        Ok(request.into_path())
    }

    fn decode_path_pair_request(
        payload: &[u8],
        operation: &'static str,
    ) -> Result<PathPairRequest, DispatchError> {
        postcard::from_bytes::<PathPairRequest>(payload)
            .map_err(|source| DispatchError::Decode { operation, source })
    }

    async fn list_directory(path: &str) -> Result<Vec<DirectoryEntry>, DispatchError> {
        let components = normalized_components(path)?;
        if components.is_empty() {
            return read_directory_entries(root_descriptor()?, path).await;
        }

        let (parent, name) = open_parent_directory(path).await?;
        let stat = parent
            .stat_at(PathFlags::empty(), name.clone())
            .await
            .map_err(|code| filesystem_error(path, "inspect", code))?;

        if matches!(stat.type_, DescriptorType::Directory) {
            let descriptor = parent
                .open_at(
                    PathFlags::empty(),
                    name,
                    OpenFlags::DIRECTORY,
                    DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
                )
                .await
                .map_err(|code| filesystem_error(path, "open directory", code))?;
            return read_directory_entries(descriptor, path).await;
        }

        Ok(vec![DirectoryEntry {
            name: display_name(path),
            kind: convert_entry_kind(stat.type_),
        }])
    }

    async fn read_directory_entries(
        descriptor: Descriptor,
        path: &str,
    ) -> Result<Vec<DirectoryEntry>, DispatchError> {
        let (stream, result) = descriptor.read_directory();
        let mut entries = stream
            .collect()
            .await
            .into_iter()
            .map(convert_directory_entry)
            .collect::<Vec<_>>();
        result
            .await
            .map_err(|code| filesystem_error(path, "read directory", code))?;
        entries.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(entries)
    }

    async fn read_file(path: &str) -> Result<Vec<u8>, DispatchError> {
        let descriptor = open_path(path, DescriptorFlags::READ, OpenFlags::empty()).await?;
        let descriptor_type = descriptor
            .get_type()
            .await
            .map_err(|code| filesystem_error(path, "inspect", code))?;
        if matches!(descriptor_type, DescriptorType::Directory) {
            return Err(DispatchError::Filesystem(format!(
                "debugger filesystem path {path} is a directory"
            )));
        }
        let (stream, result) = descriptor.read_via_stream(0);
        let bytes = stream.collect().await;
        result
            .await
            .map_err(|code| filesystem_error(path, "read file", code))?;
        Ok(bytes)
    }

    async fn remove_path(path: &str) -> Result<(), DispatchError> {
        let (parent, name) = open_parent_directory(path).await?;
        let stat = parent
            .stat_at(PathFlags::empty(), name.clone())
            .await
            .map_err(|code| filesystem_error(path, "inspect", code))?;
        match stat.type_ {
            DescriptorType::Directory => parent
                .remove_directory_at(name)
                .await
                .map_err(|code| filesystem_error(path, "remove directory", code))?,
            _ => parent
                .unlink_file_at(name)
                .await
                .map_err(|code| filesystem_error(path, "remove file", code))?,
        }
        Ok(())
    }

    async fn create_directory_path(path: &str) -> Result<(), DispatchError> {
        let (parent, name) = open_parent_directory(path).await?;
        parent
            .create_directory_at(name)
            .await
            .map_err(|code| filesystem_error(path, "create directory", code))?;
        Ok(())
    }

    async fn touch_path(path: &str) -> Result<(), DispatchError> {
        let (parent, name) = open_parent_directory(path).await?;
        let _ = parent
            .open_at(
                PathFlags::empty(),
                name,
                OpenFlags::CREATE,
                DescriptorFlags::WRITE,
            )
            .await
            .map_err(|code| filesystem_error(path, "touch", code))?;
        Ok(())
    }

    async fn write_file(path: &str, bytes: &[u8], append: bool) -> Result<(), DispatchError> {
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
                .map_err(|code| filesystem_error(path, "stat", code))?
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
                    .map_err(|code| filesystem_error(path, "write file", code))
            },
            async move {
                tx.write(bytes).await;
                drop(tx);
                Ok::<(), std::io::Error>(())
            },
        )
        .await;
        feed_result.map_err(|source| DispatchError::Io {
            operation: "filesystem.write",
            source,
        })?;
        write_result
    }

    async fn copy_path(source: &str, destination: &str) -> Result<(), DispatchError> {
        if source == destination {
            return Err(DispatchError::protocol(
                "debugger filesystem copy requires distinct source and destination paths",
            ));
        }

        let descriptor = open_path(source, DescriptorFlags::READ, OpenFlags::empty()).await?;
        let descriptor_type = descriptor
            .get_type()
            .await
            .map_err(|code| filesystem_error(source, "inspect", code))?;
        if matches!(descriptor_type, DescriptorType::Directory) {
            return Err(DispatchError::Filesystem(format!(
                "debugger filesystem copy does not support directories: {source}"
            )));
        }

        let (stream, result) = descriptor.read_via_stream(0);
        let bytes = stream.collect().await;
        result
            .await
            .map_err(|code| filesystem_error(source, "read file", code))?;
        write_file(destination, &bytes, false).await
    }

    async fn move_guest_path(source: &str, destination: &str) -> Result<(), DispatchError> {
        if source == destination {
            return Err(DispatchError::protocol(
                "debugger filesystem move requires distinct source and destination paths",
            ));
        }

        let (source_parent, source_name) = open_parent_directory(source).await?;
        let (destination_parent, destination_name) = open_parent_directory(destination).await?;
        source_parent
            .rename_at(source_name, &destination_parent, destination_name)
            .await
            .map_err(|code| filesystem_error(source, "move", code))?;
        Ok(())
    }

    fn normalized_components(path: &str) -> Result<Vec<String>, DispatchError> {
        let mut segments = Vec::new();
        for component in Path::new(path).components() {
            match component {
                std::path::Component::RootDir | std::path::Component::CurDir => {}
                std::path::Component::Normal(segment) => segments.push(
                    segment
                        .to_str()
                        .ok_or_else(|| {
                            DispatchError::protocol(format!(
                                "path {path:?} contains a non-utf8 segment"
                            ))
                        })?
                        .to_owned(),
                ),
                std::path::Component::ParentDir => {
                    return Err(DispatchError::protocol(format!(
                        "path {path:?} contains unsupported parent traversal"
                    )));
                }
                std::path::Component::Prefix(_) => {
                    return Err(DispatchError::protocol(format!(
                        "path {path:?} uses an unsupported path prefix"
                    )));
                }
            }
        }
        Ok(segments)
    }

    fn root_descriptor() -> Result<Descriptor, DispatchError> {
        preopens::get_directories()
            .into_iter()
            .find_map(|(descriptor, path)| (path == "/").then_some(descriptor))
            .ok_or_else(|| {
                DispatchError::protocol("debugger is missing a preopened root directory")
            })
    }

    async fn open_path(
        path: &str,
        flags: DescriptorFlags,
        open_flags: OpenFlags,
    ) -> Result<Descriptor, DispatchError> {
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
                .map_err(|code| filesystem_error(path, "open", code))?;
        }

        Ok(descriptor)
    }

    async fn open_parent_directory(path: &str) -> Result<(Descriptor, String), DispatchError> {
        let mut components = normalized_components(path)?;
        let name = components.pop().ok_or_else(|| {
            DispatchError::protocol(format!("path {path:?} does not refer to a removable entry"))
        })?;
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
                .map_err(|code| filesystem_error(path, "open parent directory for", code))?;
        }

        Ok((descriptor, name))
    }

    fn filesystem_error(path: &str, action: &str, code: ErrorCode) -> DispatchError {
        DispatchError::Filesystem(format!(
            "failed to {action} {path}: {}",
            describe_error_code(code)
        ))
    }

    fn describe_error_code(code: ErrorCode) -> &'static str {
        match code {
            ErrorCode::BadDescriptor => "bad file descriptor",
            ErrorCode::Busy => "resource busy",
            ErrorCode::Exist => "file exists",
            ErrorCode::Invalid => "invalid argument",
            ErrorCode::IsDirectory => "is a directory",
            ErrorCode::NoEntry => "no such file or directory",
            ErrorCode::NotDirectory => "not a directory",
            ErrorCode::NotEmpty => "directory not empty",
            ErrorCode::NotPermitted => "operation not permitted",
            ErrorCode::Overflow => "value overflow",
            ErrorCode::ReadOnly => "read-only filesystem",
            ErrorCode::Unsupported => "operation not supported",
            _ => "filesystem error",
        }
    }

    fn convert_directory_entry(entry: WasiDirectoryEntry) -> DirectoryEntry {
        DirectoryEntry {
            name: entry.name,
            kind: convert_entry_kind(entry.type_),
        }
    }

    fn convert_entry_kind(kind: DescriptorType) -> EntryKind {
        match kind {
            DescriptorType::Directory => EntryKind::Directory,
            DescriptorType::RegularFile => EntryKind::File,
            _ => EntryKind::Other,
        }
    }

    fn display_name(path: &str) -> String {
        Path::new(path)
            .file_name()
            .unwrap_or_else(|| Path::new(path).as_os_str())
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(feature = "guest")]
pub(crate) use guest::{dispatch, supports};
