//! User-program exec and spawn interfaces.
//!
//! `spawn` is the low-level entry point: it launches a program artifact and
//! returns a handle whose `stdin`/`stdout`/`stderr` are byte streams that
//! can be piped from/into another stream endpoint. `exec` is the
//! convenience "run to completion with buffered output" wrapper, and `aot`
//! produces a signed `cwasm` artifact at a caller-chosen destination path.

use crate::bindings::helios::system::programs as raw;
use crate::bindings::wit_stream;
use crate::wit_bindgen::{FutureReader, StreamReader};
use futures_lite::future::zip;

pub use crate::bindings::helios::system::programs::{
    AotHint, AotRequest, AotResult, CapabilityGrant, ClockGrant, ClockRight, DirectoryGrant,
    ExecError, ExecErrorKind, ExecOutput, ExecRequest, ExecResult, ExitStatus, FilesystemRight,
    LinkGrant, LinkRight, NetworkGrant, NetworkRight, ProcessGrant, ProcessRight, SpawnError,
    SpawnErrorKind, SpawnRequest, TerminalGrant, TerminalRight,
};

/// Build a directory capability grant for `helios:system/programs`.
pub fn directory_grant(
    source_path: impl Into<String>,
    guest_name: impl Into<String>,
    rights: Vec<FilesystemRight>,
) -> CapabilityGrant {
    CapabilityGrant::Directory(DirectoryGrant {
        source_path: source_path.into(),
        guest_name: guest_name.into(),
        rights,
    })
}

/// Full root preopen grant for trusted system programs such as the shell.
pub fn root_directory_grant() -> CapabilityGrant {
    directory_grant(
        "/",
        "/",
        vec![
            FilesystemRight::Read,
            FilesystemRight::Write,
            FilesystemRight::MutateDirectory,
            FilesystemRight::Execute,
        ],
    )
}

pub fn network_grant(rights: Vec<NetworkRight>) -> CapabilityGrant {
    CapabilityGrant::Network(NetworkGrant { rights })
}

pub fn clock_grant(rights: Vec<ClockRight>) -> CapabilityGrant {
    CapabilityGrant::Clock(ClockGrant { rights })
}

pub fn terminal_grant(rights: Vec<TerminalRight>) -> CapabilityGrant {
    CapabilityGrant::Terminal(TerminalGrant { rights })
}

pub fn root_terminal_grant() -> CapabilityGrant {
    terminal_grant(vec![
        TerminalRight::Input,
        TerminalRight::Output,
        TerminalRight::Control,
    ])
}

pub fn process_grant(rights: Vec<ProcessRight>) -> CapabilityGrant {
    CapabilityGrant::Process(ProcessGrant { rights })
}

pub fn link_grant(rights: Vec<LinkRight>) -> CapabilityGrant {
    CapabilityGrant::Link(LinkGrant { rights })
}

pub fn root_link_grant() -> CapabilityGrant {
    link_grant(vec![
        LinkRight::Source,
        LinkRight::TargetDirectory,
        LinkRight::SymlinkCreate,
        LinkRight::SymlinkRead,
    ])
}

pub fn root_process_grant() -> CapabilityGrant {
    process_grant(vec![
        ProcessRight::Spawn,
        ProcessRight::Exec,
        ProcessRight::Fork,
        ProcessRight::Join,
        ProcessRight::Signal,
    ])
}

/// Owned handle to a spawned child program.
///
/// The handle is the userland wrapper around the `helios:system/programs.child`
/// WIT resource. It exposes ergonomic Rust APIs for reading the child's
/// stdout/stderr, feeding it stdin, and waiting for its exit status.
pub struct Child {
    inner: raw::Child,
}

impl Child {
    /// Launch a program and return a live [`Child`] handle.
    pub async fn spawn(request: SpawnRequest) -> Result<Self, SpawnError> {
        let inner = raw::spawn(request).await?;
        Ok(Self { inner })
    }

    /// Pipe `bytes` into the child's stdin and close the writer. Returns
    /// when the delivery completes.
    pub async fn write_stdin(&self, bytes: Vec<u8>) -> Result<(), ()> {
        let (mut tx, rx) = wit_stream::new::<u8>();
        let future = self.inner.stdin(rx);
        let produce = async move {
            if !bytes.is_empty() {
                let _ = tx.write_all(bytes).await;
            }
            drop(tx);
        };
        let ((), result) = zip(produce, std::future::IntoFuture::into_future(future)).await;
        result
    }

    /// Collect the child's entire stdout as a `Vec<u8>`.
    pub async fn read_stdout(&self) -> Vec<u8> {
        drain_stream(self.inner.stdout()).await
    }

    /// Collect the child's entire stderr as a `Vec<u8>`.
    pub async fn read_stderr(&self) -> Vec<u8> {
        drain_stream(self.inner.stderr()).await
    }

    /// Return the child's raw stdout byte stream reader plus the
    /// completion future. Useful for piping the child's stdout directly
    /// into another program's stdin without buffering.
    pub fn stdout(&self) -> (StreamReader<u8>, FutureReader<Result<(), ()>>) {
        self.inner.stdout()
    }

    /// Return the child's raw stderr byte stream reader plus the
    /// completion future.
    pub fn stderr(&self) -> (StreamReader<u8>, FutureReader<Result<(), ()>>) {
        self.inner.stderr()
    }

    /// Pipe a stream into the child's stdin. Returns the completion
    /// future.
    pub fn pipe_stdin(&self, data: StreamReader<u8>) -> FutureReader<Result<(), ()>> {
        self.inner.stdin(data)
    }

    /// Await child completion.
    pub async fn wait(self) -> Result<ExitStatus, SpawnError> {
        self.inner.wait().await
    }
}

async fn drain_stream(pair: (StreamReader<u8>, FutureReader<Result<(), ()>>)) -> Vec<u8> {
    let (mut stream, future) = pair;
    let mut collected = Vec::new();
    const CHUNK: usize = 16 * 1024;
    loop {
        let buf = Vec::with_capacity(CHUNK);
        let (result, chunk) = stream.read(buf).await;
        if !chunk.is_empty() {
            collected.extend_from_slice(&chunk);
        }
        if crate::stream_closed(result) {
            break;
        }
    }
    let _ = std::future::IntoFuture::into_future(future).await;
    collected
}

/// Launch a program by path and wait for it to finish, collecting its
/// stdout/stderr buffers.
pub async fn exec(request: ExecRequest) -> Result<ExecResult, ExecError> {
    raw::exec(request).await
}

/// Spawn a program and return the live handle.
pub async fn spawn(request: SpawnRequest) -> Result<Child, SpawnError> {
    Child::spawn(request).await
}

/// Ahead-of-time compile a raw program artifact into a signed `cwasm` file.
pub async fn aot(request: AotRequest) -> Result<AotResult, ExecError> {
    raw::aot(request).await
}
