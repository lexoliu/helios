use std::collections::{HashMap, HashSet, VecDeque};
use std::future::{Future, poll_fn};
use std::io;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use futures_io::{AsyncRead as FuturesAsyncRead, AsyncWrite as FuturesAsyncWrite};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::sync::futures::Notified;
use tokio::sync::{Mutex, Notify, mpsc};

use crate::error::TransportError;
use crate::wire::{Frame, read_frame, write_frame};

/// Transport-level result with structured error provenance.
///
/// Every caller of this transport — the inspector, the guest debugger and
/// this crate's own tests — uses this alias, so a failure keeps the
/// variant that names what the transport was doing.
pub type Result<T> = core::result::Result<T, TransportError>;

type IoFuture<T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'static>>;
type ServeFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
type InvocationAccept<R, W> = Result<((), Outgoing<R, W>, Incoming<R, W>)>;
const RAW_UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct InboundBuffers {
    chunks: HashMap<Vec<usize>, VecDeque<Bytes>>,
    closed: HashSet<Vec<usize>>,
    error: Option<String>,
}

enum InvocationOwner<R, W> {
    Client(Weak<ClientInner<R, W>>),
    Server(Weak<ServerInner<R, W>>),
}

/// One call in flight, and the only place its answer is ever filed.
///
/// Everything a caller waits for — the open reply, inbound payloads,
/// the close of a path, a failure — lives on this object, and every
/// mutation of it signals `progress`. A waiter therefore watches the
/// same object the reader writes to, so an answer cannot be filed
/// somewhere the caller waiting for it is not looking.
struct Invocation<R, W> {
    id: u32,
    owner: InvocationOwner<R, W>,
    inbound: StdMutex<InboundBuffers>,
    accepted: AtomicBool,
    /// The reply to this invocation's `Open`. A server invocation is
    /// created already accepted and never carries one.
    reply: StdMutex<Option<Reply>>,
    /// Broadcast to every waiter each time this invocation's state
    /// changes. Signalled with `notify_waiters`, which stores no
    /// permit, so a waiter arms before it tests and never wakes on a
    /// change it has already seen.
    progress: Notify,
}

#[derive(Clone)]
pub struct Client<R, W> {
    inner: Arc<ClientInner<R, W>>,
}

/// The client half of a connection: one reader, many waiters.
///
/// Whichever caller holds `read` is the reader, and it routes the frame
/// it read into the invocation that frame names before it releases the
/// lock. Every other caller parks on its own invocation's signal and
/// races that signal against the same lock, so the reader role passes
/// to a waiter as soon as the current reader steps away, and a waiter
/// whose answer has already been filed leaves through its signal rather
/// than taking the transport.
struct ClientInner<R, W> {
    read: Arc<Mutex<R>>,
    write: Arc<Mutex<W>>,
    closed: StdMutex<bool>,
    next_invocation: AtomicU32,
    active: StdMutex<HashMap<u32, Weak<Invocation<R, W>>>>,
}

#[derive(Clone)]
pub struct Server<R, W> {
    inner: Arc<ServerInner<R, W>>,
}

struct ServerInner<R, W> {
    read: Arc<Mutex<R>>,
    write: Arc<Mutex<W>>,
    dispatch: Arc<Mutex<()>>,
    registrations: StdMutex<HashMap<(String, String), Registration<R, W>>>,
    active: StdMutex<HashMap<u32, Weak<Invocation<R, W>>>>,
    closed: StdMutex<bool>,
}

#[derive(Clone)]
struct Registration<R, W> {
    tx: mpsc::Sender<InvocationAccept<R, W>>,
}

/// How the remote answered an `Open`.
enum Reply {
    Accept,
    Reject(String),
}

pub struct Incoming<R, W> {
    invocation: Arc<Invocation<R, W>>,
    path: Arc<[usize]>,
    state: StdMutex<IncomingState>,
}

#[derive(Default)]
struct IncomingState {
    current: Option<Bytes>,
    pending: Option<IoFuture<()>>,
}

pub struct Outgoing<R, W> {
    invocation: Arc<Invocation<R, W>>,
    path: Arc<[usize]>,
    state: StdMutex<OutgoingState>,
}

#[derive(Default)]
struct OutgoingState {
    pending_write: Option<(usize, IoFuture<()>)>,
    pending_close: Option<IoFuture<()>>,
    closed: bool,
}

pub struct InvocationStream<R, W> {
    server: Arc<ServerInner<R, W>>,
    rx: mpsc::Receiver<InvocationAccept<R, W>>,
    pending: Option<ServeFuture>,
}

impl<R, W> Client<R, W>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    pub fn new(read: R, write: W) -> Self {
        Self {
            inner: Arc::new(ClientInner {
                read: Arc::new(Mutex::new(read)),
                write: Arc::new(Mutex::new(write)),
                closed: StdMutex::new(false),
                next_invocation: AtomicU32::new(1),
                active: StdMutex::new(HashMap::new()),
            }),
        }
    }

    pub async fn invoke_raw(
        &self,
        instance: &str,
        func: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let chunk_bytes = payload.len().max(1);
        self.invoke_raw_chunked(instance, func, payload, chunk_bytes)
            .await
    }

    pub async fn invoke_raw_chunked(
        &self,
        instance: &str,
        func: &str,
        payload: Vec<u8>,
        chunk_bytes: usize,
    ) -> Result<Vec<u8>> {
        let chunk_bytes = chunk_bytes.max(1);
        let (mut outgoing, mut incoming) =
            self.open_invocation(instance, func, Bytes::new()).await?;
        for chunk in payload.chunks(chunk_bytes) {
            outgoing
                .write_all(chunk)
                .await
                .map_err(|source| TransportError::io("stream raw request payload", source))?;
        }
        outgoing
            .shutdown()
            .await
            .map_err(|source| TransportError::io("close raw request channel", source))?;
        let mut response = Vec::new();
        incoming
            .read_to_end(&mut response)
            .await
            .map_err(|source| TransportError::io("read raw response channel", source))?;
        Ok(response)
    }

    pub async fn invoke_raw_streaming(
        &self,
        instance: &str,
        func: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        self.invoke_raw_chunked(instance, func, payload, RAW_UPLOAD_CHUNK_BYTES)
            .await
    }

    pub async fn open_invocation(
        &self,
        instance: &str,
        func: &str,
        params: Bytes,
    ) -> Result<(Outgoing<R, W>, Incoming<R, W>)> {
        let id = self.inner.next_invocation.fetch_add(1, Ordering::Relaxed);
        let invocation = Invocation::new_client(id, &self.inner);
        {
            let mut io = self.inner.write.lock().await;
            write_frame(
                &mut *io,
                &Frame::Open {
                    invocation: id,
                    instance: instance.to_owned(),
                    func: func.to_owned(),
                },
            )
            .await
            .map_err(|source| TransportError::io("open remote invocation", source))?;
        }

        loop {
            // Armed before the reply is tested for, so a reply filed
            // between the test and the park still wakes this caller.
            let mut signal = pin!(invocation.progress.notified());
            signal.as_mut().enable();

            if let Some(reply) = invocation.take_reply() {
                match reply {
                    Reply::Accept => break,
                    Reply::Reject(message) => return Err(TransportError::Rejected(message)),
                }
            }
            if is_closed(&self.inner.closed) {
                return Err(TransportError::Closed);
            }
            drive_client(&self.inner, signal)
                .await
                .map_err(|source| TransportError::io("read remote invocation reply", source))?;
        }

        if !params.is_empty() {
            invocation
                .clone()
                .write_data(Vec::new(), params)
                .await
                .map_err(|source| TransportError::io("transmit synchronous parameters", source))?;
        }

        Ok((
            Outgoing::root(invocation.clone()),
            Incoming::root(invocation),
        ))
    }
}

impl<R, W> Server<R, W>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    pub fn new(read: R, write: W) -> Self {
        Self {
            inner: Arc::new(ServerInner {
                read: Arc::new(Mutex::new(read)),
                write: Arc::new(Mutex::new(write)),
                dispatch: Arc::new(Mutex::new(())),
                registrations: StdMutex::new(HashMap::new()),
                active: StdMutex::new(HashMap::new()),
                closed: StdMutex::new(false),
            }),
        }
    }

    pub fn is_closed(&self) -> bool {
        is_closed(&self.inner.closed)
    }

    pub fn serve(&self, instance: &str, func: &str) -> Result<InvocationStream<R, W>> {
        let key = (instance.to_owned(), func.to_owned());
        let (tx, rx) = mpsc::channel(8);
        let previous = self
            .inner
            .registrations
            .lock()
            .unwrap_or_else(|_| panic!("server registration table mutex poisoned"))
            .insert(key.clone(), Registration { tx });
        assert!(
            previous.is_none(),
            "duplicate handler registration for {}.{}",
            key.0,
            key.1
        );
        Ok(InvocationStream {
            server: self.inner.clone(),
            rx,
            pending: None,
        })
    }
}

impl<R, W> Invocation<R, W> {
    fn new_client(id: u32, inner: &Arc<ClientInner<R, W>>) -> Arc<Self> {
        let invocation = Arc::new(Self {
            id,
            owner: InvocationOwner::Client(Arc::downgrade(inner)),
            inbound: StdMutex::new(InboundBuffers::default()),
            accepted: AtomicBool::new(false),
            reply: StdMutex::new(None),
            progress: Notify::new(),
        });
        register_invocation(&inner.active, &invocation);
        invocation
    }

    fn new_server(id: u32, inner: &Arc<ServerInner<R, W>>) -> Arc<Self> {
        let invocation = Arc::new(Self {
            id,
            owner: InvocationOwner::Server(Arc::downgrade(inner)),
            inbound: StdMutex::new(InboundBuffers::default()),
            accepted: AtomicBool::new(true),
            reply: StdMutex::new(None),
            progress: Notify::new(),
        });
        register_invocation(&inner.active, &invocation);
        invocation
    }

    fn push_payload(&self, path: Vec<usize>, payload: Bytes) {
        {
            let mut inbound = self
                .inbound
                .lock()
                .unwrap_or_else(|_| panic!("invocation inbound buffer mutex poisoned"));
            inbound.chunks.entry(path).or_default().push_back(payload);
        }
        self.progress.notify_waiters();
    }

    fn mark_closed(&self, path: Vec<usize>) {
        {
            let mut inbound = self
                .inbound
                .lock()
                .unwrap_or_else(|_| panic!("invocation inbound buffer mutex poisoned"));
            inbound.closed.insert(path);
        }
        self.progress.notify_waiters();
    }

    fn pop_payload(&self, path: &[usize]) -> Option<Bytes> {
        let mut inbound = self
            .inbound
            .lock()
            .unwrap_or_else(|_| panic!("invocation inbound buffer mutex poisoned"));
        let queue = inbound.chunks.get_mut(path)?;
        let chunk = queue.pop_front();
        if queue.is_empty() {
            inbound.chunks.remove(path);
        }
        chunk
    }

    fn has_payload(&self, path: &[usize]) -> bool {
        self.inbound
            .lock()
            .unwrap_or_else(|_| panic!("invocation inbound buffer mutex poisoned"))
            .chunks
            .get(path)
            .is_some_and(|queue| !queue.is_empty())
    }

    fn is_closed(&self, path: &[usize]) -> bool {
        self.inbound
            .lock()
            .unwrap_or_else(|_| panic!("invocation inbound buffer mutex poisoned"))
            .closed
            .contains(path)
    }

    fn mark_accepted(&self) {
        self.accepted.store(true, Ordering::Release);
    }

    /// Files the answer to this invocation's `Open` and wakes whoever
    /// opened it.
    fn deliver_reply(&self, reply: Reply) {
        *self
            .reply
            .lock()
            .unwrap_or_else(|_| panic!("invocation reply mutex poisoned")) = Some(reply);
        self.progress.notify_waiters();
    }

    fn take_reply(&self) -> Option<Reply> {
        self.reply
            .lock()
            .unwrap_or_else(|_| panic!("invocation reply mutex poisoned"))
            .take()
    }

    fn is_accepted(&self) -> bool {
        self.accepted.load(Ordering::Acquire)
    }

    /// Fails this invocation, keeping the first failure reported: the
    /// remote's own message says more than the connection teardown that
    /// follows it.
    fn set_error(&self, message: String) {
        {
            let mut inbound = self
                .inbound
                .lock()
                .unwrap_or_else(|_| panic!("invocation inbound buffer mutex poisoned"));
            if inbound.error.is_none() {
                inbound.error = Some(message);
            }
        }
        self.progress.notify_waiters();
    }

    /// `Some` once `path` has something for its reader, `None` while
    /// the caller must keep waiting.
    fn ready_state(&self, path: &[usize]) -> Option<io::Result<()>> {
        if self.has_payload(path) || self.is_closed(path) {
            return Some(Ok(()));
        }
        self.error_message()
            .map(|message| Err(io::Error::other(message)))
    }

    fn error_message(&self) -> Option<String> {
        self.inbound
            .lock()
            .unwrap_or_else(|_| panic!("invocation inbound buffer mutex poisoned"))
            .error
            .clone()
    }
}

impl<R, W> Drop for Invocation<R, W> {
    fn drop(&mut self) {
        self.owner.remove_invocation(self.id);
    }
}

impl<R, W> InvocationOwner<R, W> {
    fn remove_invocation(&self, id: u32) {
        match self {
            Self::Client(inner) => {
                if let Some(inner) = inner.upgrade() {
                    remove_invocation(&inner.active, id);
                }
            }
            Self::Server(inner) => {
                if let Some(inner) = inner.upgrade() {
                    remove_invocation(&inner.active, id);
                }
            }
        }
    }
}

impl<R, W> Invocation<R, W>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    async fn read_until_ready(self: Arc<Self>, path: Vec<usize>) -> io::Result<()> {
        loop {
            match &self.owner {
                InvocationOwner::Client(inner) => {
                    // Armed before the state is tested, so a frame
                    // routed between the test and the park still wakes
                    // this waiter.
                    let mut signal = pin!(self.progress.notified());
                    signal.as_mut().enable();

                    if let Some(state) = self.ready_state(&path) {
                        return state;
                    }
                    let inner = inner.upgrade().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "client transport disappeared")
                    })?;
                    drive_client(&inner, signal).await?;
                }
                InvocationOwner::Server(inner) => {
                    if let Some(state) = self.ready_state(&path) {
                        return state;
                    }
                    let inner = inner.upgrade().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "server transport disappeared")
                    })?;
                    pump_server_once(inner).await.map_err(io::Error::other)?;
                }
            }
        }
    }

    async fn write_data(self: Arc<Self>, path: Vec<usize>, payload: Bytes) -> io::Result<()> {
        let frame = Frame::Data {
            invocation: self.id,
            path: encode_path(&path)?,
            payload: payload.to_vec(),
        };
        match &self.owner {
            InvocationOwner::Client(inner) => {
                write_owned_frame(&inner.upgrade(), &frame, "client transport disappeared").await
            }
            InvocationOwner::Server(inner) => {
                write_owned_frame(&inner.upgrade(), &frame, "server transport disappeared").await
            }
        }
    }

    async fn close_path(self: Arc<Self>, path: Vec<usize>) -> io::Result<()> {
        let frame = Frame::Close {
            invocation: self.id,
            path: encode_path(&path)?,
        };
        match &self.owner {
            InvocationOwner::Client(inner) => {
                write_owned_frame(&inner.upgrade(), &frame, "client transport disappeared").await
            }
            InvocationOwner::Server(inner) => {
                write_owned_frame(&inner.upgrade(), &frame, "server transport disappeared").await
            }
        }
    }
}

impl<R, W> Incoming<R, W> {
    fn root(invocation: Arc<Invocation<R, W>>) -> Self {
        Self {
            invocation,
            path: Arc::from([]),
            state: StdMutex::new(IncomingState::default()),
        }
    }
}

impl<R, W> Outgoing<R, W> {
    fn root(invocation: Arc<Invocation<R, W>>) -> Self {
        Self {
            invocation,
            path: Arc::from([]),
            state: StdMutex::new(OutgoingState::default()),
        }
    }
}

impl<R, W> AsyncRead for Incoming<R, W>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        loop {
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|_| panic!("incoming state mutex poisoned"));
                if let Some(current) = state.current.as_mut() {
                    let count = current.len().min(buf.remaining());
                    buf.put_slice(&current.split_to(count));
                    if current.is_empty() {
                        state.current = None;
                    }
                    return Poll::Ready(Ok(()));
                }
            }

            if let Some(chunk) = self.invocation.pop_payload(self.path.as_ref()) {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|_| panic!("incoming state mutex poisoned"));
                state.current = Some(chunk);
                continue;
            }

            if self.invocation.is_closed(self.path.as_ref()) {
                return Poll::Ready(Ok(()));
            }
            if let Some(message) = self.invocation.error_message() {
                return Poll::Ready(Err(io::Error::other(message)));
            }

            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|_| panic!("incoming state mutex poisoned"));
                if state.pending.is_none() {
                    state.pending = Some(Box::pin(
                        self.invocation
                            .clone()
                            .read_until_ready(self.path.as_ref().to_vec()),
                    ));
                }
                let future = state
                    .pending
                    .as_mut()
                    .unwrap_or_else(|| panic!("incoming pending future disappeared"));
                match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(())) => {
                        state.pending = None;
                    }
                    Poll::Ready(Err(error)) => {
                        state.pending = None;
                        return Poll::Ready(Err(error));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
        }
    }
}

impl<R, W> AsyncWrite for Outgoing<R, W>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("outgoing state mutex poisoned"));
        if state.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "attempted to write to a closed invocation stream",
            )));
        }

        if state.pending_write.is_none() {
            let chunk_len = buf.len().min(RAW_UPLOAD_CHUNK_BYTES);
            let bytes = Bytes::copy_from_slice(&buf[..chunk_len]);
            state.pending_write = Some((
                chunk_len,
                Box::pin(
                    self.invocation
                        .clone()
                        .write_data(self.path.as_ref().to_vec(), bytes),
                ),
            ));
        }

        let (written, future) = state
            .pending_write
            .as_mut()
            .unwrap_or_else(|| panic!("outgoing pending write disappeared"));
        match future.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => {
                let written = *written;
                state.pending_write = None;
                Poll::Ready(Ok(written))
            }
            Poll::Ready(Err(error)) => {
                state.pending_write = None;
                Poll::Ready(Err(error))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("outgoing state mutex poisoned"));
        if state.closed {
            return Poll::Ready(Ok(()));
        }

        if let Some((_, future)) = state.pending_write.as_mut() {
            match future.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    state.pending_write = None;
                }
                Poll::Ready(Err(error)) => {
                    state.pending_write = None;
                    return Poll::Ready(Err(error));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        if state.pending_close.is_none() {
            state.pending_close = Some(Box::pin(
                self.invocation
                    .clone()
                    .close_path(self.path.as_ref().to_vec()),
            ));
        }

        let future = state
            .pending_close
            .as_mut()
            .unwrap_or_else(|| panic!("outgoing pending close disappeared"));
        match future.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => {
                state.pending_close = None;
                state.closed = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => {
                state.pending_close = None;
                Poll::Ready(Err(error))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<R, W> Stream for InvocationStream<R, W>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    type Item = Result<((), Outgoing<R, W>, Incoming<R, W>)>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.rx.poll_recv(cx) {
                Poll::Ready(Some(item)) => return Poll::Ready(Some(item)),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => {}
            }

            if is_closed(&this.server.closed) {
                return Poll::Ready(None);
            }

            if this.pending.is_none() {
                this.pending = Some(Box::pin(pump_server_once(this.server.clone())));
            }

            let future = this
                .pending
                .as_mut()
                .unwrap_or_else(|| panic!("server pump future disappeared"));
            match future.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    this.pending = None;
                }
                Poll::Ready(Err(error)) => {
                    this.pending = None;
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

fn register_invocation<R, W>(
    active: &StdMutex<HashMap<u32, Weak<Invocation<R, W>>>>,
    invocation: &Arc<Invocation<R, W>>,
) {
    active
        .lock()
        .unwrap_or_else(|_| panic!("active invocation table mutex poisoned"))
        .insert(invocation.id, Arc::downgrade(invocation));
}

fn remove_invocation<R, W>(active: &StdMutex<HashMap<u32, Weak<Invocation<R, W>>>>, id: u32) {
    active
        .lock()
        .unwrap_or_else(|_| panic!("active invocation table mutex poisoned"))
        .remove(&id);
}

fn dispatch_payload<R, W>(
    active: &StdMutex<HashMap<u32, Weak<Invocation<R, W>>>>,
    invocation: u32,
    path: Vec<usize>,
    payload: Bytes,
) {
    if let Some(target) = resolve_invocation(active, invocation) {
        target.push_payload(path, payload);
    }
}

fn dispatch_close<R, W>(
    active: &StdMutex<HashMap<u32, Weak<Invocation<R, W>>>>,
    invocation: u32,
    path: Vec<usize>,
) {
    if let Some(target) = resolve_invocation(active, invocation) {
        target.mark_closed(path);
    }
}

fn resolve_invocation<R, W>(
    active: &StdMutex<HashMap<u32, Weak<Invocation<R, W>>>>,
    invocation: u32,
) -> Option<Arc<Invocation<R, W>>> {
    let mut active = active
        .lock()
        .unwrap_or_else(|_| panic!("active invocation table mutex poisoned"));
    match active.get(&invocation).and_then(Weak::upgrade) {
        Some(target) => Some(target),
        None => {
            active.remove(&invocation);
            None
        }
    }
}

fn is_closed(closed: &StdMutex<bool>) -> bool {
    *closed
        .lock()
        .unwrap_or_else(|_| panic!("transport closed flag mutex poisoned"))
}

fn mark_closed(closed: &StdMutex<bool>) {
    let mut closed = closed
        .lock()
        .unwrap_or_else(|_| panic!("transport closed flag mutex poisoned"));
    *closed = true;
}

/// Waits for `signal`, or takes the reader role and routes one frame.
///
/// The two are raced with the signal polled first, and the reader
/// routes its frame before it releases the read lock. A caller whose
/// answer has already been filed therefore always finds the signal
/// ready on the poll that would otherwise hand it the transport, so it
/// cannot end up inside `read_frame` waiting for a frame the remote has
/// already sent.
async fn drive_client<R, W>(
    client: &Arc<ClientInner<R, W>>,
    mut signal: Pin<&mut Notified<'_>>,
) -> io::Result<()>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    if is_closed(&client.closed) {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "client transport is closed",
        ));
    }

    let mut acquire = pin!(client.read.lock());
    let reader = poll_fn(|cx| {
        if signal.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        acquire.as_mut().poll(cx).map(Some)
    })
    .await;
    let Some(mut io) = reader else {
        return Ok(());
    };

    let frame = match read_frame(&mut *io).await {
        Ok(Some(frame)) => frame,
        Ok(None) => {
            close_client(client, "client transport closed mid-invocation");
            return Ok(());
        }
        Err(error) => {
            close_client(client, &error.to_string());
            return Err(error);
        }
    };
    route_client_frame(client, frame).inspect_err(|error| {
        close_client(client, &error.to_string());
    })
}

/// Files one frame into the invocation it names.
///
/// A frame for an invocation whose caller has dropped it is discarded:
/// the caller registers the invocation before it writes the `Open` and
/// holds it for as long as it waits, so the only frames that resolve to
/// nothing are answers nobody is waiting for.
fn route_client_frame<R, W>(client: &Arc<ClientInner<R, W>>, frame: Frame) -> io::Result<()> {
    match frame {
        Frame::Accept { invocation } => {
            if let Some(target) = resolve_invocation(&client.active, invocation) {
                target.mark_accepted();
                target.deliver_reply(Reply::Accept);
            }
        }
        Frame::Reject {
            invocation,
            message,
        } => {
            if let Some(target) = resolve_invocation(&client.active, invocation) {
                if target.is_accepted() {
                    target.set_error(message);
                } else {
                    target.deliver_reply(Reply::Reject(message));
                }
            }
        }
        Frame::Data {
            invocation,
            path,
            payload,
        } => dispatch_payload(
            &client.active,
            invocation,
            decode_path(&path)?,
            Bytes::from(payload),
        ),
        Frame::Close { invocation, path } => {
            dispatch_close(&client.active, invocation, decode_path(&path)?);
        }
        Frame::Open { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "client transport received unexpected open frame",
            ));
        }
    }

    Ok(())
}

/// Ends every call on a connection the reader can no longer read.
///
/// With several invocations in flight the failure belongs to all of
/// them, not only to whichever one happened to hold the reader role:
/// failing each one both reports the fault and wakes the waiter parked
/// on its signal.
fn close_client<R, W>(client: &Arc<ClientInner<R, W>>, reason: &str) {
    mark_closed(&client.closed);
    let waiting: Vec<Arc<Invocation<R, W>>> = client
        .active
        .lock()
        .unwrap_or_else(|_| panic!("active invocation table mutex poisoned"))
        .values()
        .filter_map(Weak::upgrade)
        .collect();
    for invocation in waiting {
        invocation.set_error(reason.to_owned());
    }
}

async fn pump_server_once<R, W>(server: Arc<ServerInner<R, W>>) -> Result<()>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    if is_closed(&server.closed) {
        return Ok(());
    }

    let handoff = {
        let _dispatch = server.dispatch.lock().await;
        let frame = {
            let mut io = server.read.lock().await;
            match read_frame(&mut *io)
                .await
                .map_err(|source| TransportError::io("read server transport frame", source))?
            {
                Some(frame) => frame,
                None => {
                    mark_server_closed(&server);
                    return Ok(());
                }
            }
        };

        match frame {
            Frame::Open {
                invocation,
                instance,
                func,
            } => {
                let registration = server
                    .registrations
                    .lock()
                    .unwrap_or_else(|_| panic!("server registration table mutex poisoned"))
                    .get(&(instance.clone(), func.clone()))
                    .map(|registration| Registration {
                        tx: registration.tx.clone(),
                    });

                match registration {
                    Some(registration) => {
                        {
                            let mut io = server.write.lock().await;
                            write_frame(&mut *io, &Frame::Accept { invocation })
                                .await
                                .map_err(|source| {
                                    TransportError::io("accept remote invocation", source)
                                })?;
                        }

                        let invocation = Invocation::new_server(invocation, &server);
                        Some((
                            registration,
                            Ok((
                                (),
                                Outgoing::root(invocation.clone()),
                                Incoming::root(invocation),
                            )),
                            instance,
                            func,
                        ))
                    }
                    None => {
                        let mut io = server.write.lock().await;
                        write_frame(
                            &mut *io,
                            &Frame::Reject {
                                invocation,
                                message: format!("no handler registered for {instance}.{func}"),
                            },
                        )
                        .await
                        .map_err(|source| TransportError::io("reject remote invocation", source))?;
                        None
                    }
                }
            }
            Frame::Data {
                invocation,
                path,
                payload,
            } => {
                dispatch_payload(
                    &server.active,
                    invocation,
                    decode_path(&path).map_err(|source| {
                        TransportError::io("decode incoming data path", source)
                    })?,
                    Bytes::from(payload),
                );
                None
            }
            Frame::Close { invocation, path } => {
                dispatch_close(
                    &server.active,
                    invocation,
                    decode_path(&path).map_err(|source| {
                        TransportError::io("decode incoming close path", source)
                    })?,
                );
                None
            }
            Frame::Accept { .. } | Frame::Reject { .. } => {
                return Err(TransportError::UnexpectedReply);
            }
        }
    };

    if let Some((registration, invocation, instance, func)) = handoff {
        registration
            .tx
            .send(invocation)
            .await
            .map_err(|_| TransportError::HandoffClosed { instance, func })?;
    }

    Ok(())
}

fn mark_server_closed<R, W>(server: &ServerInner<R, W>) {
    mark_closed(&server.closed);
    server
        .registrations
        .lock()
        .unwrap_or_else(|_| panic!("server registration table mutex poisoned"))
        .clear();
    server
        .active
        .lock()
        .unwrap_or_else(|_| panic!("active invocation table mutex poisoned"))
        .clear();
}

async fn write_owned_frame<W, I>(
    owner: &Option<Arc<I>>,
    frame: &Frame,
    disappeared: &'static str,
) -> io::Result<()>
where
    W: FuturesAsyncWrite + Send + Unpin + 'static,
    I: FrameWriter<W>,
{
    let owner = owner
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, disappeared))?;
    let mut io = owner.write().lock().await;
    write_frame(&mut *io, frame).await
}

trait FrameWriter<W> {
    fn write(&self) -> &Arc<Mutex<W>>;
}

impl<R, W> FrameWriter<W> for ClientInner<R, W> {
    fn write(&self) -> &Arc<Mutex<W>> {
        &self.write
    }
}

impl<R, W> FrameWriter<W> for ServerInner<R, W> {
    fn write(&self) -> &Arc<Mutex<W>> {
        &self.write
    }
}

fn encode_path(path: &[usize]) -> io::Result<Vec<u32>> {
    path.iter()
        .map(|index| {
            u32::try_from(*index)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
        })
        .collect()
}

fn decode_path(path: &[u32]) -> io::Result<Vec<usize>> {
    path.iter()
        .map(|index| {
            usize::try_from(*index)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Client, RAW_UPLOAD_CHUNK_BYTES, Server};
    use bytes::Bytes;
    use futures_lite::StreamExt as _;
    use futures_lite::future::poll_once;
    use futures_util::future::join;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    /// How long the second of two concurrently held invocations may go
    /// unanswered. Long enough that a loaded machine does not fail it,
    /// short enough that a client which serializes its waiters fails
    /// the test instead of hanging the suite.
    const CONCURRENT_DEADLINE: Duration = Duration::from_secs(5);

    #[test]
    fn sequential_empty_invocations_do_not_stall() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async {
                let (host, peer) = tokio::io::duplex(4096);
                let (host_read, host_write) = tokio::io::split(host);
                let server = Server::new(host_read.compat(), host_write.compat_write());
                let (peer_read, peer_write) = tokio::io::split(peer);
                let client = Client::new(peer_read.compat(), peer_write.compat_write());

                let mut invocations = server
                    .serve("transport:test", "ping")
                    .unwrap_or_else(|error| panic!("failed to register server handler: {error}"));
                let server_task = tokio::spawn(async move {
                    for response in ["one", "two"] {
                        let (_, mut outgoing, mut incoming) = invocations
                            .next()
                            .await
                            .unwrap_or_else(|| panic!("server invocation stream ended early"))
                            .unwrap_or_else(|error| {
                                panic!("failed to accept server invocation: {error}")
                            });
                        let mut request = Vec::new();
                        incoming
                            .read_to_end(&mut request)
                            .await
                            .unwrap_or_else(|error| {
                                panic!("failed to read request payload: {error}")
                            });
                        assert!(
                            request.is_empty(),
                            "unexpected request payload: {request:?}"
                        );
                        outgoing
                            .write_all(response.as_bytes())
                            .await
                            .unwrap_or_else(|error| {
                                panic!("failed to write response payload: {error}")
                            });
                        outgoing.shutdown().await.unwrap_or_else(|error| {
                            panic!("failed to close response stream: {error}")
                        });
                    }
                });

                assert_eq!(
                    client
                        .invoke_raw("transport:test", "ping", Vec::new())
                        .await
                        .unwrap_or_else(|error| panic!("first raw invocation failed: {error}")),
                    b"one"
                );
                assert_eq!(
                    client
                        .invoke_raw("transport:test", "ping", Vec::new())
                        .await
                        .unwrap_or_else(|error| panic!("second raw invocation failed: {error}")),
                    b"two"
                );

                server_task
                    .await
                    .unwrap_or_else(|error| panic!("server task panicked: {error}"));
            });
    }

    #[test]
    fn active_invocation_does_not_block_next_call() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async {
                let (host, peer) = tokio::io::duplex(4096);
                let (host_read, host_write) = tokio::io::split(host);
                let server = Server::new(host_read.compat(), host_write.compat_write());
                let (peer_read, peer_write) = tokio::io::split(peer);
                let client = Client::new(peer_read.compat(), peer_write.compat_write());
                let released = std::sync::Arc::new(Notify::new());

                let mut stream_calls = server
                    .serve("transport:test", "stream")
                    .unwrap_or_else(|error| panic!("failed to register stream handler: {error}"));
                let stream_release = released.clone();
                let stream_task = tokio::spawn(async move {
                    let (_, mut outgoing, mut incoming) = stream_calls
                        .next()
                        .await
                        .unwrap_or_else(|| panic!("stream invocation stream ended early"))
                        .unwrap_or_else(|error| {
                            panic!("failed to accept stream invocation: {error}")
                        });
                    let mut request = Vec::new();
                    incoming
                        .read_to_end(&mut request)
                        .await
                        .unwrap_or_else(|error| panic!("failed to read stream request: {error}"));
                    assert!(
                        request.is_empty(),
                        "unexpected stream request payload: {request:?}"
                    );
                    outgoing
                        .write_all(b"stream")
                        .await
                        .unwrap_or_else(|error| panic!("failed to write stream response: {error}"));
                    stream_release.notified().await;
                    outgoing
                        .shutdown()
                        .await
                        .unwrap_or_else(|error| panic!("failed to close stream response: {error}"));
                });

                let mut ping_calls = server
                    .serve("transport:test", "ping")
                    .unwrap_or_else(|error| panic!("failed to register ping handler: {error}"));
                let ping_release = released.clone();
                let ping_task =
                    tokio::spawn(async move {
                        let (_, mut outgoing, mut incoming) = ping_calls
                            .next()
                            .await
                            .unwrap_or_else(|| panic!("ping invocation stream ended early"))
                            .unwrap_or_else(|error| {
                                panic!("failed to accept ping invocation: {error}")
                            });
                        let mut request = Vec::new();
                        incoming
                            .read_to_end(&mut request)
                            .await
                            .unwrap_or_else(|error| panic!("failed to read ping request: {error}"));
                        assert!(
                            request.is_empty(),
                            "unexpected ping request payload: {request:?}"
                        );
                        outgoing.write_all(b"pong").await.unwrap_or_else(|error| {
                            panic!("failed to write ping response: {error}")
                        });
                        outgoing.shutdown().await.unwrap_or_else(|error| {
                            panic!("failed to close ping response: {error}")
                        });
                        ping_release.notify_one();
                    });

                let (mut first_outgoing, mut first_incoming) = client
                    .open_invocation("transport:test", "stream", Bytes::new())
                    .await
                    .unwrap_or_else(|error| panic!("failed to open stream invocation: {error}"));
                first_outgoing.shutdown().await.unwrap_or_else(|error| {
                    panic!("failed to close stream request channel: {error}")
                });

                assert_eq!(
                    client
                        .invoke_raw("transport:test", "ping", Vec::new())
                        .await
                        .unwrap_or_else(|error| panic!("ping invocation failed: {error}")),
                    b"pong"
                );

                let mut response = Vec::new();
                first_incoming
                    .read_to_end(&mut response)
                    .await
                    .unwrap_or_else(|error| panic!("failed to read stream response: {error}"));
                assert_eq!(response, b"stream");

                stream_task
                    .await
                    .unwrap_or_else(|error| panic!("stream task panicked: {error}"));
                ping_task
                    .await
                    .unwrap_or_else(|error| panic!("ping task panicked: {error}"));
            });
    }

    #[test]
    fn accepted_invocation_can_fail_without_tearing_down_transport() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async {
                let (host, peer) = tokio::io::duplex(4096);
                let (host_read, host_write) = tokio::io::split(host);
                let server = Server::new(host_read.compat(), host_write.compat_write());
                let (peer_read, peer_write) = tokio::io::split(peer);
                let client = Client::new(peer_read.compat(), peer_write.compat_write());

                let mut calls = server
                    .serve("transport:test", "fail")
                    .unwrap_or_else(|error| panic!("failed to register fail handler: {error}"));
                let server_task = tokio::spawn(async move {
                    let (_, _outgoing, mut incoming) = calls
                        .next()
                        .await
                        .unwrap_or_else(|| panic!("fail invocation stream ended early"))
                        .unwrap_or_else(|error| {
                            panic!("failed to accept fail invocation: {error}")
                        });
                    let mut request = Vec::new();
                    incoming
                        .read_to_end(&mut request)
                        .await
                        .unwrap_or_else(|error| panic!("failed to read fail request: {error}"));
                    assert_eq!(request, b"request");

                    let mut io = server.inner.write.lock().await;
                    super::write_frame(
                        &mut *io,
                        &super::Frame::Reject {
                            invocation: 1,
                            message: "remote failure".to_owned(),
                        },
                    )
                    .await
                    .unwrap_or_else(|error| panic!("failed to write reject frame: {error}"));
                });

                let error = client
                    .invoke_raw("transport:test", "fail", b"request".to_vec())
                    .await
                    .expect_err("accepted invocation must surface remote failure");
                assert!(
                    format!("{error:#}").contains("remote failure"),
                    "unexpected error: {error:#}"
                );

                server_task
                    .await
                    .unwrap_or_else(|error| panic!("fail server task panicked: {error}"));
            });
    }

    /// Runs one call per entry of `responses`, answering each with the
    /// bytes it names, and gives back what the client read.
    ///
    /// The payload shape is the point of every test that uses it. The
    /// transport carries opaque bytes, so a response of one byte, of
    /// several, and of bytes that are all zero each have to reach the
    /// caller exactly as the server wrote them — a length the framing
    /// got wrong, or a zero byte read as "nothing was sent", would be
    /// invisible to a test that only checks that a call completed.
    fn round_trip(func: &'static str, responses: &[&'static [u8]]) -> Vec<Vec<u8>> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async move {
                let (host, peer) = tokio::io::duplex(4096);
                let (host_read, host_write) = tokio::io::split(host);
                let server = Server::new(host_read.compat(), host_write.compat_write());
                let (peer_read, peer_write) = tokio::io::split(peer);
                let client = Client::new(peer_read.compat(), peer_write.compat_write());

                let mut invocations =
                    server
                        .serve("transport:test", func)
                        .unwrap_or_else(|error| {
                            panic!("failed to register the {func} handler: {error}")
                        });
                let answers = responses.to_vec();
                let server_task = tokio::spawn(async move {
                    for response in answers {
                        let (_, mut outgoing, mut incoming) = invocations
                            .next()
                            .await
                            .unwrap_or_else(|| panic!("the {func} stream ended early"))
                            .unwrap_or_else(|error| {
                                panic!("failed to accept a {func} invocation: {error}")
                            });
                        let mut request = Vec::new();
                        incoming
                            .read_to_end(&mut request)
                            .await
                            .unwrap_or_else(|error| {
                                panic!("failed to read the {func} request: {error}")
                            });
                        assert!(
                            request.is_empty(),
                            "unexpected {func} request payload: {request:?}"
                        );
                        outgoing.write_all(response).await.unwrap_or_else(|error| {
                            panic!("failed to write the {func} response: {error}")
                        });
                        outgoing.shutdown().await.unwrap_or_else(|error| {
                            panic!("failed to close the {func} response stream: {error}")
                        });
                    }
                });

                let mut read = Vec::new();
                for _ in responses {
                    read.push(
                        client
                            .invoke_raw("transport:test", func, Vec::new())
                            .await
                            .unwrap_or_else(|error| {
                                panic!("the {func} invocation failed: {error}")
                            }),
                    );
                }
                server_task
                    .await
                    .unwrap_or_else(|error| panic!("the {func} server task panicked: {error}"));
                read
            })
    }

    /// Two calls in a row each keep their own answer, down to a response
    /// that is a single byte.
    #[test]
    fn sequential_invocations_keep_their_own_short_answers() {
        assert_eq!(
            round_trip("unit", &[&[7], &[11]]),
            vec![vec![7_u8], vec![11_u8]]
        );
    }

    /// A one-byte response is not an empty one. The byte a caller sends
    /// to mean "no value" is still a byte the transport has to deliver,
    /// and a framing that treated it as an empty payload would hand the
    /// caller nothing to decode.
    #[test]
    fn a_single_byte_response_is_not_read_as_an_empty_one() {
        assert_eq!(round_trip("resource", &[&[0]]), vec![vec![0_u8]]);
    }

    /// A response of several bytes arrives with all of them, in order.
    #[test]
    fn a_multi_byte_response_arrives_whole() {
        assert_eq!(
            round_trip("resource-some", &[&[1, 4, 0x12, 0x34, 0x56, 0x78]]),
            vec![vec![1, 4, 0x12, 0x34, 0x56, 0x78]]
        );
    }

    /// A response whose payload bytes are all zero is still a response.
    /// Nothing in the framing may read a zero byte as an absent one.
    #[test]
    fn an_all_zero_response_payload_arrives_whole() {
        assert_eq!(
            round_trip("resource-zero", &[&[1, 4, 0, 0, 0, 0]]),
            vec![vec![1, 4, 0, 0, 0, 0]]
        );
    }

    #[test]
    fn large_raw_response_is_chunked_across_frames() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async {
                let (host, peer) = tokio::io::duplex(4096);
                let (host_read, host_write) = tokio::io::split(host);
                let server = Server::new(host_read.compat(), host_write.compat_write());
                let (peer_read, peer_write) = tokio::io::split(peer);
                let client = Client::new(peer_read.compat(), peer_write.compat_write());

                let expected = vec![0x5a; RAW_UPLOAD_CHUNK_BYTES * 3 + 17];
                let mut calls = server
                    .serve("transport:test", "large-response")
                    .unwrap_or_else(|error| {
                        panic!("failed to register large-response handler: {error}")
                    });
                let expected_response = expected.clone();
                let server_task = tokio::spawn(async move {
                    let (_, mut outgoing, mut incoming) = calls
                        .next()
                        .await
                        .unwrap_or_else(|| panic!("large-response invocation stream ended early"))
                        .unwrap_or_else(|error| {
                            panic!("failed to accept large-response invocation: {error}")
                        });
                    let mut request = Vec::new();
                    incoming
                        .read_to_end(&mut request)
                        .await
                        .unwrap_or_else(|error| {
                            panic!("failed to read large-response request: {error}")
                        });
                    assert!(
                        request.is_empty(),
                        "unexpected request payload: {request:?}"
                    );
                    outgoing
                        .write_all(&expected_response)
                        .await
                        .unwrap_or_else(|error| {
                            panic!("failed to write large-response payload: {error}")
                        });
                    outgoing.shutdown().await.unwrap_or_else(|error| {
                        panic!("failed to close large-response stream: {error}")
                    });
                });

                let actual = client
                    .invoke_raw("transport:test", "large-response", Vec::new())
                    .await
                    .unwrap_or_else(|error| panic!("large-response invocation failed: {error}"));
                assert_eq!(actual, expected);

                server_task
                    .await
                    .unwrap_or_else(|error| panic!("large-response server task panicked: {error}"));
            });
    }

    #[test]
    fn queued_open_does_not_block_active_invocation_progress() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async {
                let (host, peer) = tokio::io::duplex(4096);
                let (host_read, host_write) = tokio::io::split(host);
                let server = Server::new(host_read.compat(), host_write.compat_write());
                let (peer_read, peer_write) = tokio::io::split(peer);
                let mut peer_read = peer_read.compat();
                let mut peer_write = peer_write.compat_write();

                let mut invocations = server
                    .serve("transport:test", "queue")
                    .unwrap_or_else(|error| panic!("failed to register queue handler: {error}"));

                super::write_frame(
                    &mut peer_write,
                    &super::Frame::Open {
                        invocation: 1,
                        instance: "transport:test".to_owned(),
                        func: "queue".to_owned(),
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("failed to write first open frame: {error}"));
                super::pump_server_once(server.inner.clone())
                    .await
                    .unwrap_or_else(|error| panic!("failed to accept first invocation: {error}"));
                match super::read_frame(&mut peer_read)
                    .await
                    .unwrap_or_else(|error| panic!("failed to read first accept frame: {error}"))
                {
                    Some(super::Frame::Accept { invocation }) => assert_eq!(invocation, 1),
                    other => panic!("unexpected first accept frame: {other:?}"),
                }

                let (_, _first_outgoing, mut first_incoming) = invocations
                    .next()
                    .await
                    .unwrap_or_else(|| panic!("first invocation stream ended early"))
                    .unwrap_or_else(|error| panic!("failed to receive first invocation: {error}"));

                for invocation in 2..=9 {
                    super::write_frame(
                        &mut peer_write,
                        &super::Frame::Open {
                            invocation,
                            instance: "transport:test".to_owned(),
                            func: "queue".to_owned(),
                        },
                    )
                    .await
                    .unwrap_or_else(|error| {
                        panic!("failed to write queued open frame {invocation}: {error}")
                    });
                    super::pump_server_once(server.inner.clone())
                        .await
                        .unwrap_or_else(|error| {
                            panic!("failed to pump queued open frame {invocation}: {error}")
                        });
                    match super::read_frame(&mut peer_read)
                        .await
                        .unwrap_or_else(|error| {
                            panic!("failed to read queued accept frame {invocation}: {error}")
                        }) {
                        Some(super::Frame::Accept {
                            invocation: accepted,
                        }) => {
                            assert_eq!(accepted, invocation);
                        }
                        other => panic!(
                            "unexpected queued accept frame for invocation {invocation}: {other:?}"
                        ),
                    }
                }

                super::write_frame(
                    &mut peer_write,
                    &super::Frame::Open {
                        invocation: 10,
                        instance: "transport:test".to_owned(),
                        func: "queue".to_owned(),
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("failed to write blocked open frame: {error}"));

                let blocked_pump = tokio::spawn({
                    let server = server.inner.clone();
                    async move { super::pump_server_once(server).await }
                });

                match super::read_frame(&mut peer_read)
                    .await
                    .unwrap_or_else(|error| panic!("failed to read blocked accept frame: {error}"))
                {
                    Some(super::Frame::Accept { invocation }) => assert_eq!(invocation, 10),
                    other => panic!("unexpected blocked accept frame: {other:?}"),
                }

                tokio::task::yield_now().await;

                super::write_frame(
                    &mut peer_write,
                    &super::Frame::Data {
                        invocation: 1,
                        path: Vec::new(),
                        payload: b"request".to_vec(),
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("failed to write active request data: {error}"));
                super::write_frame(
                    &mut peer_write,
                    &super::Frame::Close {
                        invocation: 1,
                        path: Vec::new(),
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("failed to write active request close: {error}"));

                let mut request = Vec::new();
                timeout(
                    Duration::from_millis(100),
                    first_incoming.read_to_end(&mut request),
                )
                .await
                .unwrap_or_else(|_| panic!("active invocation stalled behind queued open"))
                .unwrap_or_else(|error| {
                    panic!("failed to read active invocation payload: {error}")
                });
                assert_eq!(request, b"request");

                let _ = invocations
                    .next()
                    .await
                    .unwrap_or_else(|| panic!("queued invocation stream ended unexpectedly"))
                    .unwrap_or_else(|error| panic!("failed to release queued invocation: {error}"));

                blocked_pump
                    .await
                    .unwrap_or_else(|error| panic!("blocked pump task panicked: {error}"))
                    .unwrap_or_else(|error| panic!("blocked pump failed: {error}"));
            });
    }

    /// Two invocations held open at once must both complete.
    ///
    /// One caller reads the connection while the other waits, so the
    /// reader routes the waiter's answer as it goes. A client whose
    /// waiters take turns at the transport files that answer and then
    /// parks the waiter in `read_frame` for a frame the guest has
    /// already sent, and neither call ever finishes.
    #[test]
    fn concurrently_held_invocations_both_complete() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async {
                let (host, peer) = tokio::io::duplex(4096);
                let (host_read, host_write) = tokio::io::split(host);
                let server = Server::new(host_read.compat(), host_write.compat_write());
                let (peer_read, peer_write) = tokio::io::split(peer);
                let client = Client::new(peer_read.compat(), peer_write.compat_write());
                let release = std::sync::Arc::new(Notify::new());

                let mut slow_calls = server
                    .serve("transport:test", "slow")
                    .unwrap_or_else(|error| panic!("failed to register slow handler: {error}"));
                let slow_release = release.clone();
                let slow_task =
                    tokio::spawn(async move {
                        let (_, mut outgoing, mut incoming) = slow_calls
                            .next()
                            .await
                            .unwrap_or_else(|| panic!("slow invocation stream ended early"))
                            .unwrap_or_else(|error| {
                                panic!("failed to accept slow invocation: {error}")
                            });
                        let mut request = Vec::new();
                        incoming
                            .read_to_end(&mut request)
                            .await
                            .unwrap_or_else(|error| panic!("failed to read slow request: {error}"));
                        // The guest holds this call open while it answers
                        // the other one, which is the whole point: the host
                        // must be able to use the connection meanwhile.
                        slow_release.notified().await;
                        outgoing.write_all(b"slow").await.unwrap_or_else(|error| {
                            panic!("failed to write slow response: {error}")
                        });
                        outgoing.shutdown().await.unwrap_or_else(|error| {
                            panic!("failed to close slow response: {error}")
                        });
                    });

                let mut probe_calls = server
                    .serve("transport:test", "probe")
                    .unwrap_or_else(|error| panic!("failed to register probe handler: {error}"));
                let probe_task = tokio::spawn(async move {
                    let (_, mut outgoing, mut incoming) = probe_calls
                        .next()
                        .await
                        .unwrap_or_else(|| panic!("probe invocation stream ended early"))
                        .unwrap_or_else(|error| {
                            panic!("failed to accept probe invocation: {error}")
                        });
                    let mut request = Vec::new();
                    incoming
                        .read_to_end(&mut request)
                        .await
                        .unwrap_or_else(|error| panic!("failed to read probe request: {error}"));
                    outgoing
                        .write_all(b"pong")
                        .await
                        .unwrap_or_else(|error| panic!("failed to write probe response: {error}"));
                    outgoing
                        .shutdown()
                        .await
                        .unwrap_or_else(|error| panic!("failed to close probe response: {error}"));
                });

                let (mut slow_outgoing, mut slow_incoming) = client
                    .open_invocation("transport:test", "slow", Bytes::new())
                    .await
                    .unwrap_or_else(|error| panic!("failed to open slow invocation: {error}"));
                slow_outgoing.shutdown().await.unwrap_or_else(|error| {
                    panic!("failed to close slow request channel: {error}")
                });

                let held = async {
                    let mut response = Vec::new();
                    slow_incoming
                        .read_to_end(&mut response)
                        .await
                        .unwrap_or_else(|error| panic!("failed to read slow response: {error}"));
                    assert_eq!(response, b"slow");
                };
                let concurrent = async {
                    let answer = timeout(
                        CONCURRENT_DEADLINE,
                        client.invoke_raw("transport:test", "probe", Vec::new()),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        panic!(
                            "the second invocation went unanswered for {CONCURRENT_DEADLINE:?} \
                             while the first was held open"
                        )
                    })
                    .unwrap_or_else(|error| panic!("probe invocation failed: {error}"));
                    assert_eq!(answer, b"pong");
                    release.notify_one();
                };
                join(held, concurrent).await;

                slow_task
                    .await
                    .unwrap_or_else(|error| panic!("slow server task panicked: {error}"));
                probe_task
                    .await
                    .unwrap_or_else(|error| panic!("probe server task panicked: {error}"));
            });
    }

    /// An answer filed before its caller waits for it must reach it.
    ///
    /// The whole of one invocation's answer — accept, payload and
    /// close — is routed by the caller that holds the read half, the
    /// payload and close while nobody is waiting on them at all. The
    /// wire is empty by the time that caller reads its own response, so
    /// it completes only if it is delivered what was filed for it
    /// rather than sent back to the transport for a frame that will
    /// never arrive.
    #[test]
    fn an_answer_filed_while_its_caller_parks_is_delivered() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("failed to build test runtime: {error}"))
            .block_on(async {
                let (guest, peer) = tokio::io::duplex(4096);
                let (guest_read, guest_write) = tokio::io::split(guest);
                let mut guest_read = guest_read.compat();
                let mut guest_write = guest_write.compat_write();
                let (peer_read, peer_write) = tokio::io::split(peer);
                let client = Client::new(peer_read.compat(), peer_write.compat_write());

                // The first caller takes the reader role and parks in
                // the transport; the second is left a pure waiter.
                let mut reader =
                    Box::pin(client.open_invocation("transport:test", "reader", Bytes::new()));
                assert!(
                    poll_once(&mut reader).await.is_none(),
                    "the first caller must park until the guest answers it"
                );
                let mut waiter =
                    Box::pin(client.open_invocation("transport:test", "waiter", Bytes::new()));
                assert!(
                    poll_once(&mut waiter).await.is_none(),
                    "the second caller must park until the guest answers it"
                );

                for expected in ["reader", "waiter"] {
                    match super::read_frame(&mut guest_read)
                        .await
                        .unwrap_or_else(|error| {
                            panic!("failed to read the {expected} open frame: {error}")
                        }) {
                        Some(super::Frame::Open { func, .. }) => assert_eq!(func, expected),
                        other => panic!("unexpected {expected} open frame: {other:?}"),
                    }
                }

                for frame in [
                    super::Frame::Accept { invocation: 2 },
                    super::Frame::Data {
                        invocation: 2,
                        path: Vec::new(),
                        payload: b"answered".to_vec(),
                    },
                    super::Frame::Close {
                        invocation: 2,
                        path: Vec::new(),
                    },
                    super::Frame::Accept { invocation: 1 },
                ] {
                    super::write_frame(&mut guest_write, &frame)
                        .await
                        .unwrap_or_else(|error| panic!("failed to write {frame:?}: {error}"));
                }

                let (reading, parked) = timeout(CONCURRENT_DEADLINE, join(reader, waiter))
                    .await
                    .unwrap_or_else(|_| panic!("a caller never saw the accept filed for it"));
                let (_reader_outgoing, _reader_incoming) = reading
                    .unwrap_or_else(|error| panic!("the reading invocation failed: {error}"));
                let (_waiter_outgoing, mut waiter_incoming) =
                    parked.unwrap_or_else(|error| panic!("the parked invocation failed: {error}"));

                // Nothing is left on the wire, so this response can
                // only come from what was filed while nobody waited.
                let mut response = Vec::new();
                timeout(
                    CONCURRENT_DEADLINE,
                    waiter_incoming.read_to_end(&mut response),
                )
                .await
                .unwrap_or_else(|_| panic!("the parked caller never saw the payload filed for it"))
                .unwrap_or_else(|error| panic!("failed to read the parked response: {error}"));
                assert_eq!(response, b"answered");
            });
    }
}
