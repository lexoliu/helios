//! User-program exec & spawn syscalls.
//!
//! `spawn` is the low-level entry point: it launches a wasm program and
//! returns a handle whose `stdin`/`stdout`/`stderr` are byte streams that
//! can be piped from/into anything that speaks `wasi:io`. `exec` is the
//! convenience "run to completion with buffered output" wrapper, and `aot`
//! produces a signed `cwasm` artifact at a caller-chosen destination path.

use crate::bindings::helios::system::programs as raw;
use crate::bindings::wit_stream;
use crate::wit_bindgen::{FutureReader, StreamReader};
use futures_lite::future::zip;

pub use crate::bindings::helios::system::programs::{
    AotHint, AotRequest, AotResult, ExecError, ExecErrorKind, ExecOutput, ExecRequest, ExecResult,
    ExitStatus, SpawnError, SpawnErrorKind, SpawnRequest,
};

/// Owned handle to a spawned child wasm program.
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
    use crate::wit_bindgen::rt::async_support::StreamResult;
    let (mut stream, future) = pair;
    let mut collected = Vec::new();
    const CHUNK: usize = 16 * 1024;
    loop {
        let buf = Vec::with_capacity(CHUNK);
        let (result, chunk) = stream.read(buf).await;
        if !chunk.is_empty() {
            collected.extend_from_slice(&chunk);
        }
        if matches!(
            result,
            StreamResult::Dropped | StreamResult::Cancelled | StreamResult::Complete(0)
        ) {
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

/// Ahead-of-time compile a raw wasm program into a signed `cwasm` file.
pub async fn aot(request: AotRequest) -> Result<AotResult, ExecError> {
    raw::aot(request).await
}
