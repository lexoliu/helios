extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

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
