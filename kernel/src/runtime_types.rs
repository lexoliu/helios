extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use core::future::Future;

use crate::InstanceId;
use helios_hal::io::IoError;

#[derive(Clone, Debug)]
pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct ExecResult {
    pub instance_id: InstanceId,
    pub exit_code: u32,
    pub output: ExecOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PingErrorKind {
    UnresolvedHost,
    Timeout,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug)]
pub struct PingError {
    pub kind: PingErrorKind,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Address {
    octets: [u8; 4],
}

impl Ipv4Address {
    pub const fn new(octets: [u8; 4]) -> Self {
        Self { octets }
    }

    pub const fn octets(self) -> [u8; 4] {
        self.octets
    }
}

#[derive(Clone, Debug)]
pub struct PingReply {
    pub address: Ipv4Address,
    pub round_trip_nanos: u64,
    pub payload_bytes: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpErrorKind {
    UnresolvedHost,
    Timeout,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug)]
pub struct TcpError {
    pub kind: TcpErrorKind,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct HostDirEntry {
    pub name: String,
    pub is_directory: bool,
}

#[derive(Clone, Debug)]
pub struct HostMetadata {
    pub qid_path: u64,
    pub qid_type: u8,
    pub mode: u32,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub enum HostFsError {
    Transport(IoError),
    Protocol(&'static str),
    Server(u32),
    Utf8,
}


pub trait HostFileSystem: Clone + Send + 'static {
    type StatFuture<'a>: Future<Output = Result<HostMetadata, HostFsError>> + 'a
    where
        Self: 'a;

    type ReadDirFuture<'a>: Future<Output = Result<Vec<HostDirEntry>, HostFsError>> + 'a
    where
        Self: 'a;

    type ReadFileFuture<'a>: Future<Output = Result<Vec<u8>, HostFsError>> + 'a
    where
        Self: 'a;

    type WriteFileFuture<'a>: Future<Output = Result<(), HostFsError>> + 'a
    where
        Self: 'a;

    type TruncateFileFuture<'a>: Future<Output = Result<(), HostFsError>> + 'a
    where
        Self: 'a;

    type CreateFileFuture<'a>: Future<Output = Result<(), HostFsError>> + 'a
    where
        Self: 'a;

    type CreateDirectoryFuture<'a>: Future<Output = Result<(), HostFsError>> + 'a
    where
        Self: 'a;

    type RemoveFuture<'a>: Future<Output = Result<(), HostFsError>> + 'a
    where
        Self: 'a;

    type RenameFuture<'a>: Future<Output = Result<(), HostFsError>> + 'a
    where
        Self: 'a;

    fn stat_path(&self, path: &str) -> Self::StatFuture<'_>;
    fn read_dir(&self, path: &str) -> Self::ReadDirFuture<'_>;
    fn read_file(&self, path: &str) -> Self::ReadFileFuture<'_>;
    fn write_file(&self, path: &str, offset: u64, bytes: &[u8]) -> Self::WriteFileFuture<'_>;
    fn truncate_file(&self, path: &str) -> Self::TruncateFileFuture<'_>;
    fn create_file(&self, path: &str) -> Self::CreateFileFuture<'_>;
    fn create_directory(&self, path: &str) -> Self::CreateDirectoryFuture<'_>;
    fn remove(&self, path: &str, directory: bool) -> Self::RemoveFuture<'_>;
    fn rename(&self, source: &str, destination: &str) -> Self::RenameFuture<'_>;
}
