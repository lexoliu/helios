use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::channel::oneshot;
use futures::future::{FutureExt, LocalBoxFuture};
use futures::io::{AsyncRead, AsyncWrite};
use glob::MatchOptions;
use helios_api::bindings::wit_stream;
use helios_api::programs;
use helios_api::{fs as helios_fs, io as helios_io, task as helios_task};
use tracing::debug;

use crate::error::{Result, ShellError};
use crate::streams::{InputStream, OutputStream, close, pipe, shared_buffer_file, write_all};

#[derive(Clone, Debug)]
pub struct ResolvedProgram {
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    Truncate,
    NoClobber,
    Append,
}

impl WriteMode {
    fn appends(self) -> bool {
        matches!(self, Self::Append)
    }
}

pub struct SpawnRequest {
    pub resolved: ResolvedProgram,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: InputStream,
    pub stdout: OutputStream,
    pub stderr: OutputStream,
}

#[async_trait]
pub trait RunningProcess {
    async fn wait(self) -> Result<u8>;
}

pub trait ShellPlatform: Clone + 'static {
    type Child: RunningProcess + 'static;

    fn spawn_task(&self, task: LocalBoxFuture<'static, ()>);

    fn stdout(&self) -> OutputStream;

    fn stderr(&self) -> OutputStream;

    fn pipe(&self) -> (InputStream, OutputStream) {
        pipe()
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>>;

    async fn open_input(&self, path: &Path) -> Result<InputStream>;

    async fn open_output(&self, path: &Path, mode: WriteMode) -> Result<OutputStream>;

    async fn open_read_write(&self, path: &Path) -> Result<(InputStream, OutputStream)>;

    async fn exists(&self, path: &Path) -> bool;

    async fn is_file(&self, path: &Path) -> bool;

    async fn is_dir(&self, path: &Path) -> bool;

    async fn expand_glob(&self, cwd: &Path, pattern: &str) -> Result<Vec<String>>;

    async fn spawn(&self, request: SpawnRequest) -> Result<Self::Child>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HeliosPlatform;

#[async_trait]
impl RunningProcess for HeliosChild {
    async fn wait(self) -> Result<u8> {
        let exit = self.child.wait().await.map_err(|error| {
            ShellError::message(format!(
                "child.wait failed: {:?}: {}",
                error.kind, error.detail
            ))
        });
        await_forwarder(self.stdin_done).await?;
        await_forwarder(self.stdout_done).await?;
        await_forwarder(self.stderr_done).await?;
        let exit = exit?;
        u8::try_from(exit.code)
            .map_err(|_| ShellError::message(format!("exit code {} does not fit in u8", exit.code)))
    }
}

impl ShellPlatform for HeliosPlatform {
    type Child = HeliosChild;

    fn spawn_task(&self, task: LocalBoxFuture<'static, ()>) {
        helios_task::spawn(async move {
            task.await;
        });
    }

    fn stdout(&self) -> OutputStream {
        OutputStream::new(HeliosTerminalWriter::stdout())
    }

    fn stderr(&self) -> OutputStream {
        OutputStream::new(HeliosTerminalWriter::stderr())
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        helios_fs::read(path).await.map_err(|error| {
            ShellError::message(format!("failed to read {}: {error:#}", path.display()))
        })
    }

    async fn open_input(&self, path: &Path) -> Result<InputStream> {
        Ok(InputStream::from_bytes(self.read_file(path).await?))
    }

    async fn open_output(&self, path: &Path, mode: WriteMode) -> Result<OutputStream> {
        if matches!(mode, WriteMode::NoClobber) && helios_fs::is_file(path).await {
            return Err(existing_output_error(path));
        }
        Ok(OutputStream::new(HeliosFileWriter::new(
            path.to_path_buf(),
            mode,
        )))
    }

    async fn open_read_write(&self, path: &Path) -> Result<(InputStream, OutputStream)> {
        let initial = if helios_fs::exists(path).await {
            self.read_file(path).await?
        } else {
            Vec::new()
        };
        let path = path.to_path_buf();
        Ok(shared_buffer_file(initial, move |bytes| {
            let path = path.clone();
            async move {
                helios_fs::write(&path, bytes).await.map_err(|error| {
                    io::Error::other(format!("failed to write {}: {error:#}", path.display()))
                })
            }
            .boxed_local()
        }))
    }

    async fn exists(&self, path: &Path) -> bool {
        helios_fs::exists(path).await
    }

    async fn is_file(&self, path: &Path) -> bool {
        helios_fs::is_file(path).await
    }

    async fn is_dir(&self, path: &Path) -> bool {
        helios_fs::is_dir(path).await
    }

    async fn expand_glob(&self, cwd: &Path, pattern: &str) -> Result<Vec<String>> {
        let raw_path = Path::new(pattern);
        let absolute_pattern = if raw_path.is_absolute() {
            raw_path.to_path_buf()
        } else {
            cwd.join(raw_path)
        };
        let rendered_pattern = absolute_pattern.display().to_string();
        let relative = !raw_path.is_absolute();

        let matches = match glob::glob_with(
            &rendered_pattern,
            MatchOptions {
                case_sensitive: true,
                require_literal_separator: true,
                require_literal_leading_dot: true,
            },
        ) {
            Ok(matches) => matches,
            Err(_) => return Ok(Vec::new()),
        };

        let mut expanded = Vec::new();
        for path in matches {
            let path = path.map_err(|error| {
                ShellError::message(format!(
                    "glob expansion failed for {rendered_pattern:?}: {error}"
                ))
            })?;
            expanded.push(render_glob_match(path, cwd, relative));
        }
        expanded.sort();
        Ok(expanded)
    }

    async fn spawn(&self, request: SpawnRequest) -> Result<Self::Child> {
        debug!(program = %request.resolved.path, args = ?request.args, "spawning external shell command");
        let child = programs::spawn(programs::SpawnRequest {
            name: request.resolved.path.clone(),
            args: request.args,
            env: request.env,
            path: request.resolved.path,
        })
        .await
        .map_err(|error| {
            ShellError::message(format!("spawn failed: {:?}: {}", error.kind, error.detail))
        })?;

        let stdin_done = spawn_stdin_forwarder(self, &child, request.stdin);
        let stdout_done = spawn_stdout_forwarder(self, &child, request.stdout);
        let stderr_done = spawn_stderr_forwarder(self, &child, request.stderr);

        Ok(HeliosChild {
            child,
            stdin_done,
            stdout_done,
            stderr_done,
        })
    }
}

pub struct HeliosChild {
    child: programs::Child,
    stdin_done: oneshot::Receiver<Result<()>>,
    stdout_done: oneshot::Receiver<Result<()>>,
    stderr_done: oneshot::Receiver<Result<()>>,
}

fn spawn_stdin_forwarder(
    platform: &HeliosPlatform,
    child: &programs::Child,
    mut input: InputStream,
) -> oneshot::Receiver<Result<()>> {
    let (sender, receiver) = oneshot::channel();
    let (mut tx, rx) = wit_stream::new::<u8>();
    let stdin_delivery = child.pipe_stdin(rx);

    platform.spawn_task(Box::pin(async move {
        let result = async move {
            let mut buffer = [0_u8; 8192];
            loop {
                let read = poll_read(&mut input, &mut buffer).await?;
                if read == 0 {
                    break;
                }
                let _ = tx.write_all(buffer[..read].to_vec()).await;
            }
            drop(tx);
            std::future::IntoFuture::into_future(stdin_delivery)
                .await
                .map_err(|()| ShellError::message("failed to deliver child stdin"))?;
            Ok(())
        }
        .await;
        let _ = sender.send(result);
    }));

    receiver
}

fn spawn_stdout_forwarder(
    platform: &HeliosPlatform,
    child: &programs::Child,
    output: OutputStream,
) -> oneshot::Receiver<Result<()>> {
    spawn_output_forwarder(platform, child.stdout(), output, "stdout")
}

fn spawn_stderr_forwarder(
    platform: &HeliosPlatform,
    child: &programs::Child,
    output: OutputStream,
) -> oneshot::Receiver<Result<()>> {
    spawn_output_forwarder(platform, child.stderr(), output, "stderr")
}

fn spawn_output_forwarder(
    platform: &HeliosPlatform,
    pair: (
        helios_api::wit_bindgen::StreamReader<u8>,
        helios_api::wit_bindgen::FutureReader<std::result::Result<(), ()>>,
    ),
    mut output: OutputStream,
    stream_name: &'static str,
) -> oneshot::Receiver<Result<()>> {
    let (sender, receiver) = oneshot::channel();
    platform.spawn_task(Box::pin(async move {
        let result = forward_child_output(pair, &mut output, stream_name).await;
        let _ = sender.send(result);
    }));
    receiver
}

async fn forward_child_output(
    pair: (
        helios_api::wit_bindgen::StreamReader<u8>,
        helios_api::wit_bindgen::FutureReader<std::result::Result<(), ()>>,
    ),
    output: &mut OutputStream,
    stream_name: &'static str,
) -> Result<()> {
    use helios_api::wit_bindgen::rt::async_support::StreamResult;

    let (mut stream, future) = pair;
    const CHUNK: usize = 16 * 1024;
    loop {
        let (result, chunk) = stream.read(Vec::with_capacity(CHUNK)).await;
        if !chunk.is_empty() {
            write_all(output, &chunk).await?;
        }
        match result {
            StreamResult::Complete(0) | StreamResult::Dropped => break,
            StreamResult::Complete(_) => {}
            StreamResult::Cancelled => {
                return Err(ShellError::message(format!(
                    "child {stream_name} stream was cancelled"
                )));
            }
        }
    }

    std::future::IntoFuture::into_future(future)
        .await
        .map_err(|()| ShellError::message(format!("child {stream_name} stream failed")))?;
    close(output).await?;
    Ok(())
}

async fn await_forwarder(receiver: oneshot::Receiver<Result<()>>) -> Result<()> {
    receiver
        .await
        .map_err(|_| ShellError::message("shell stream forwarder dropped"))?
}

pub(crate) fn render_glob_match(path: PathBuf, cwd: &Path, relative: bool) -> String {
    if relative {
        match path.strip_prefix(cwd) {
            Ok(relative_path) if !relative_path.as_os_str().is_empty() => {
                relative_path.display().to_string()
            }
            Ok(_) => ".".to_owned(),
            Err(_) => path.display().to_string(),
        }
    } else {
        path.display().to_string()
    }
}

pub(crate) fn existing_output_error(path: &Path) -> ShellError {
    ShellError::message(format!("cannot create {}: File exists", path.display()))
}

struct HeliosFileWriter {
    path: PathBuf,
    mode: WriteMode,
    buffer: Vec<u8>,
    initialized: bool,
    pending: Option<LocalBoxFuture<'static, io::Result<()>>>,
}

impl HeliosFileWriter {
    fn new(path: PathBuf, mode: WriteMode) -> Self {
        Self {
            path,
            mode,
            buffer: Vec::new(),
            initialized: false,
            pending: None,
        }
    }

    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(future) = self.pending.as_mut() else {
            return Poll::Ready(Ok(()));
        };

        match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                Poll::Ready(result)
            }
        }
    }
}

struct HeliosTerminalWriter {
    sink: TerminalSink,
    buffer: Vec<u8>,
    pending: Option<LocalBoxFuture<'static, io::Result<()>>>,
}

impl HeliosTerminalWriter {
    fn stdout() -> Self {
        Self::new(TerminalSink::Stdout)
    }

    fn stderr() -> Self {
        Self::new(TerminalSink::Stderr)
    }

    fn new(sink: TerminalSink) -> Self {
        Self {
            sink,
            buffer: Vec::new(),
            pending: None,
        }
    }

    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(future) = self.pending.as_mut() else {
            return Poll::Ready(Ok(()));
        };

        match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                Poll::Ready(result)
            }
        }
    }
}

impl AsyncWrite for HeliosTerminalWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.poll_pending(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }

        self.buffer.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.poll_pending(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }

        if self.buffer.is_empty() {
            return Poll::Ready(Ok(()));
        }

        let bytes = std::mem::take(&mut self.buffer);
        let sink = self.sink;
        self.pending = Some(Box::pin(async move {
            match sink {
                TerminalSink::Stdout => helios_io::stdout()
                    .write_all(bytes)
                    .await
                    .map_err(|error| io::Error::other(error.to_string())),
                TerminalSink::Stderr => helios_io::stderr()
                    .write_all(bytes)
                    .await
                    .map_err(|error| io::Error::other(error.to_string())),
            }
        }));
        self.poll_pending(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

#[derive(Clone, Copy)]
enum TerminalSink {
    Stdout,
    Stderr,
}

impl AsyncWrite for HeliosFileWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.poll_pending(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }

        self.buffer.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.poll_pending(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }

        if self.buffer.is_empty() && self.initialized {
            return Poll::Ready(Ok(()));
        }

        let bytes = std::mem::take(&mut self.buffer);
        let path = self.path.clone();
        let mode = self.mode;
        let initialized = self.initialized;
        self.initialized = true;
        self.pending = Some(Box::pin(async move {
            let mut payload = if initialized || mode.appends() {
                helios_fs::read(&path).await.unwrap_or_default()
            } else {
                Vec::new()
            };
            payload.extend_from_slice(&bytes);
            helios_fs::write(&path, payload).await.map_err(|error| {
                io::Error::other(format!("failed to write {}: {error:#}", path.display()))
            })
        }));
        self.poll_pending(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

async fn poll_read(reader: &mut InputStream, buf: &mut [u8]) -> io::Result<usize> {
    futures::future::poll_fn(|cx| Pin::new(&mut *reader).poll_read(cx, buf)).await
}
