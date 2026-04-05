use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::task::{Context, Poll};

use anyhow::{Context as _, Result, anyhow, bail};
use bytes::Bytes;
use futures_core::Stream;
use futures_io::{AsyncRead as FuturesAsyncRead, AsyncWrite as FuturesAsyncWrite};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::sync::{Mutex, mpsc};
use wrpc_transport::{Index, Invoke, Serve};

use crate::wire::{Frame, read_frame, write_frame};

type IoFuture<T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'static>>;
type ServeFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
const RAW_UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct InboundBuffers {
    chunks: HashMap<Vec<usize>, VecDeque<Bytes>>,
    closed: HashSet<Vec<usize>>,
}

enum InvocationOwner<R, W> {
    Client(Weak<ClientInner<R, W>>),
    Server(Weak<ServerInner<R, W>>),
}

struct Invocation<R, W> {
    id: u32,
    owner: InvocationOwner<R, W>,
    inbound: StdMutex<InboundBuffers>,
}

#[derive(Clone)]
pub struct Client<R, W> {
    inner: Arc<ClientInner<R, W>>,
}

struct ClientInner<R, W> {
    read: Arc<Mutex<R>>,
    write: Arc<Mutex<W>>,
    dispatch: Arc<Mutex<()>>,
    closed: StdMutex<bool>,
    next_invocation: AtomicU32,
    active: StdMutex<HashMap<u32, Weak<Invocation<R, W>>>>,
    replies: StdMutex<HashMap<u32, Reply>>,
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
    tx: mpsc::Sender<Result<((), Outgoing<R, W>, Incoming<R, W>)>>,
}

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
    rx: mpsc::Receiver<Result<((), Outgoing<R, W>, Incoming<R, W>)>>,
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
                dispatch: Arc::new(Mutex::new(())),
                closed: StdMutex::new(false),
                next_invocation: AtomicU32::new(1),
                active: StdMutex::new(HashMap::new()),
                replies: StdMutex::new(HashMap::new()),
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
        let paths: [Box<[Option<usize>]>; 0] = [];
        let (mut outgoing, mut incoming) =
            Invoke::invoke(self, (), instance, func, Bytes::new(), paths).await?;
        for chunk in payload.chunks(chunk_bytes) {
            outgoing.write_all(chunk).await.with_context(|| {
                format!("failed to stream raw request payload for {instance}.{func}")
            })?;
        }
        outgoing
            .shutdown()
            .await
            .context("failed to close raw request channel")?;
        let mut response = Vec::new();
        incoming
            .read_to_end(&mut response)
            .await
            .context("failed to read raw response channel")?;
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
}

impl<R, W> Invoke for Client<R, W>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    type Context = ();
    type Outgoing = Outgoing<R, W>;
    type Incoming = Incoming<R, W>;

    async fn invoke<P>(
        &self,
        (): Self::Context,
        instance: &str,
        func: &str,
        params: Bytes,
        _paths: impl AsRef<[P]> + Send,
    ) -> Result<(Self::Outgoing, Self::Incoming)>
    where
        P: AsRef<[Option<usize>]> + Send + Sync,
    {
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
            .with_context(|| format!("failed to open remote invocation {instance}.{func}"))?;
        }

        loop {
            if let Some(reply) = take_reply(&self.inner.replies, id) {
                match reply {
                    Reply::Accept => break,
                    Reply::Reject(message) => return Err(anyhow!(message)),
                }
            }
            if is_closed(&self.inner.closed) {
                bail!("transport closed unexpectedly");
            }
            pump_client_once(self.inner.clone())
                .await
                .with_context(|| {
                    format!("failed to read remote invocation reply for {instance}.{func}")
                })?;
        }

        if !params.is_empty() {
            invocation
                .clone()
                .write_data(Vec::new(), params)
                .await
                .with_context(|| {
                    format!("failed to transmit synchronous parameters for {instance}.{func}")
                })?;
        }

        Ok((
            Outgoing::root(invocation.clone()),
            Incoming::root(invocation),
        ))
    }
}

impl<R, W> Serve for Server<R, W>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    type Context = ();
    type Outgoing = Outgoing<R, W>;
    type Incoming = Incoming<R, W>;

    async fn serve(
        &self,
        instance: &str,
        func: &str,
        _paths: impl Into<Arc<[Box<[Option<usize>]>]>> + Send,
    ) -> Result<impl Stream<Item = Result<(Self::Context, Self::Outgoing, Self::Incoming)>> + 'static>
    {
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
        });
        register_invocation(&inner.active, &invocation);
        invocation
    }

    fn new_server(id: u32, inner: &Arc<ServerInner<R, W>>) -> Arc<Self> {
        let invocation = Arc::new(Self {
            id,
            owner: InvocationOwner::Server(Arc::downgrade(inner)),
            inbound: StdMutex::new(InboundBuffers::default()),
        });
        register_invocation(&inner.active, &invocation);
        invocation
    }

    fn push_payload(&self, path: Vec<usize>, payload: Bytes) {
        let mut inbound = self
            .inbound
            .lock()
            .unwrap_or_else(|_| panic!("invocation inbound buffer mutex poisoned"));
        inbound.chunks.entry(path).or_default().push_back(payload);
    }

    fn mark_closed(&self, path: Vec<usize>) {
        let mut inbound = self
            .inbound
            .lock()
            .unwrap_or_else(|_| panic!("invocation inbound buffer mutex poisoned"));
        inbound.closed.insert(path);
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
            if self.has_payload(&path) || self.is_closed(&path) {
                return Ok(());
            }

            match &self.owner {
                InvocationOwner::Client(inner) => {
                    let inner = inner.upgrade().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "client transport disappeared")
                    })?;
                    pump_client_once(inner).await?;
                }
                InvocationOwner::Server(inner) => {
                    let inner = inner.upgrade().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "server transport disappeared")
                    })?;
                    pump_server_once(inner)
                        .await
                        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
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

impl<R, W> Index<Self> for Incoming<R, W> {
    fn index(&self, path: &[usize]) -> Result<Self> {
        assert!(!path.is_empty(), "incoming indexed path must not be empty");
        Ok(Self {
            invocation: self.invocation.clone(),
            path: join_path(&self.path, path),
            state: StdMutex::new(IncomingState::default()),
        })
    }
}

impl<R, W> Index<Self> for Outgoing<R, W> {
    fn index(&self, path: &[usize]) -> Result<Self> {
        assert!(!path.is_empty(), "outgoing indexed path must not be empty");
        Ok(Self {
            invocation: self.invocation.clone(),
            path: join_path(&self.path, path),
            state: StdMutex::new(OutgoingState::default()),
        })
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
            let bytes = Bytes::copy_from_slice(buf);
            state.pending_write = Some((
                buf.len(),
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
    let target = {
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
    };
    if let Some(target) = target {
        target.push_payload(path, payload);
    }
}

fn dispatch_close<R, W>(
    active: &StdMutex<HashMap<u32, Weak<Invocation<R, W>>>>,
    invocation: u32,
    path: Vec<usize>,
) {
    let target = {
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
    };
    if let Some(target) = target {
        target.mark_closed(path);
    }
}

fn take_reply(replies: &StdMutex<HashMap<u32, Reply>>, invocation: u32) -> Option<Reply> {
    replies
        .lock()
        .unwrap_or_else(|_| panic!("client reply table mutex poisoned"))
        .remove(&invocation)
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

async fn pump_client_once<R, W>(client: Arc<ClientInner<R, W>>) -> io::Result<()>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    if is_closed(&client.closed) {
        return Ok(());
    }

    let _dispatch = client.dispatch.lock().await;
    let frame = {
        let mut io = client.read.lock().await;
        match read_frame(&mut *io).await? {
            Some(frame) => frame,
            None => {
                mark_closed(&client.closed);
                return Ok(());
            }
        }
    };

    match frame {
        Frame::Accept { invocation } => {
            client
                .replies
                .lock()
                .unwrap_or_else(|_| panic!("client reply table mutex poisoned"))
                .insert(invocation, Reply::Accept);
        }
        Frame::Reject {
            invocation,
            message,
        } => {
            client
                .replies
                .lock()
                .unwrap_or_else(|_| panic!("client reply table mutex poisoned"))
                .insert(invocation, Reply::Reject(message));
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
            dispatch_close(&client.active, invocation, decode_path(&path)?)
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

async fn pump_server_once<R, W>(server: Arc<ServerInner<R, W>>) -> Result<()>
where
    R: FuturesAsyncRead + Send + Unpin + 'static,
    W: FuturesAsyncWrite + Send + Unpin + 'static,
{
    if is_closed(&server.closed) {
        return Ok(());
    }

    let _dispatch = server.dispatch.lock().await;
    let frame = {
        let mut io = server.read.lock().await;
        match read_frame(&mut *io).await? {
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
                            .with_context(|| {
                                format!("failed to accept remote invocation {instance}.{func}")
                            })?;
                    }

                    let invocation = Invocation::new_server(invocation, &server);
                    registration
                        .tx
                        .send(Ok((
                            (),
                            Outgoing::root(invocation.clone()),
                            Incoming::root(invocation),
                        )))
                        .await
                        .with_context(|| {
                            format!("handler queue for {instance}.{func} was closed")
                        })?;
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
                    .with_context(|| {
                        format!("failed to reject remote invocation {instance}.{func}")
                    })?;
                }
            }
        }
        Frame::Data {
            invocation,
            path,
            payload,
        } => dispatch_payload(
            &server.active,
            invocation,
            decode_path(&path)?,
            Bytes::from(payload),
        ),
        Frame::Close { invocation, path } => {
            dispatch_close(&server.active, invocation, decode_path(&path)?)
        }
        Frame::Accept { .. } | Frame::Reject { .. } => {
            bail!("server transport received unexpected reply frame");
        }
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

fn join_path(prefix: &[usize], suffix: &[usize]) -> Arc<[usize]> {
    if prefix.is_empty() {
        Arc::from(suffix)
    } else {
        Arc::from([prefix, suffix].concat())
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
    use super::{Client, Server};
    use bytes::Bytes;
    use futures_lite::StreamExt as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::Notify;
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
    use wrpc_transport::{Invoke as _, InvokeExt as _, Serve as _, ServeExt as _};

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
                    .serve("transport:test", "ping", [])
                    .await
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
                    .serve("transport:test", "stream", [])
                    .await
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
                    .serve("transport:test", "ping", [])
                    .await
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
                    .invoke((), "transport:test", "stream", Bytes::new(), [[None; 0]; 0])
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
    fn typed_unit_invocation_completes() {
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

                let invocations = server
                    .serve_values::<(), (u32,)>("transport:test", "unit", [])
                    .await
                    .unwrap_or_else(|error| {
                        panic!("failed to register typed server handler: {error}")
                    });
                let mut invocations = Box::pin(invocations);
                let server_task = tokio::spawn(async move {
                    for expected in [7_u32, 11_u32] {
                        let (_, (), _rx, tx) = invocations
                            .as_mut()
                            .next()
                            .await
                            .unwrap_or_else(|| panic!("typed invocation stream ended early"))
                            .unwrap_or_else(|error| {
                                panic!("failed to accept typed invocation: {error}")
                            });
                        tx((expected,)).await.unwrap_or_else(|error| {
                            panic!("failed to send typed response: {error}")
                        });
                    }
                });

                let paths: [Box<[Option<usize>]>; 0] = [];
                let (value,) = client
                    .invoke_values_blocking::<Box<[Option<usize>]>, (), (u32,)>(
                        (),
                        "transport:test",
                        "unit",
                        (),
                        paths,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("typed invocation failed: {error}"));
                assert_eq!(value, 7);

                let paths: [Box<[Option<usize>]>; 0] = [];
                let (value,) = client
                    .invoke_values_blocking::<Box<[Option<usize>]>, (), (u32,)>(
                        (),
                        "transport:test",
                        "unit",
                        (),
                        paths,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("second typed invocation failed: {error}"));
                assert_eq!(value, 11);

                server_task
                    .await
                    .unwrap_or_else(|error| panic!("typed server task panicked: {error}"));
            });
    }

    #[test]
    fn typed_optional_resource_result_completes() {
        #[repr(transparent)]
        struct TestResource(());

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

                let invocations = server
                    .serve_values::<(), (Option<wrpc_transport::ResourceOwn<TestResource>>,)>(
                        "transport:test",
                        "resource",
                        [],
                    )
                    .await
                    .unwrap_or_else(|error| {
                        panic!("failed to register resource server handler: {error}")
                    });
                let mut invocations = Box::pin(invocations);
                let server_task = tokio::spawn(async move {
                    let (_, (), _rx, tx) = invocations
                        .as_mut()
                        .next()
                        .await
                        .unwrap_or_else(|| panic!("resource invocation stream ended early"))
                        .unwrap_or_else(|error| {
                            panic!("failed to accept resource invocation: {error}")
                        });
                    tx((None,)).await.unwrap_or_else(|error| {
                        panic!("failed to send resource response: {error}")
                    });
                });

                let paths: [Box<[Option<usize>]>; 0] = [];
                let (value,) = client
                    .invoke_values_blocking::<
                        Box<[Option<usize>]>,
                        (),
                        (Option<wrpc_transport::ResourceOwn<TestResource>>,),
                    >((), "transport:test", "resource", (), paths)
                    .await
                    .unwrap_or_else(|error| panic!("resource invocation failed: {error}"));
                assert!(value.is_none(), "resource response must be none");

                server_task
                    .await
                    .unwrap_or_else(|error| panic!("resource server task panicked: {error}"));
            });
    }

    #[test]
    fn typed_some_resource_result_completes() {
        #[repr(transparent)]
        struct TestResource(());

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

                let invocations = server
                    .serve_values::<(), (Option<wrpc_transport::ResourceOwn<TestResource>>,)>(
                        "transport:test",
                        "resource-some",
                        [],
                    )
                    .await
                    .unwrap_or_else(|error| {
                        panic!("failed to register resource-some server handler: {error}")
                    });
                let mut invocations = Box::pin(invocations);
                let server_task = tokio::spawn(async move {
                    let (_, (), _rx, tx) = invocations
                        .as_mut()
                        .next()
                        .await
                        .unwrap_or_else(|| panic!("resource-some invocation stream ended early"))
                        .unwrap_or_else(|error| {
                            panic!("failed to accept resource-some invocation: {error}")
                        });
                    let handle = wrpc_transport::ResourceOwn::from(Bytes::from_static(&[
                        0x12, 0x34, 0x56, 0x78,
                    ]));
                    tx((Some(handle),)).await.unwrap_or_else(|error| {
                        panic!("failed to send resource-some response: {error}")
                    });
                });

                let paths: [Box<[Option<usize>]>; 0] = [];
                let (value,) = client
                    .invoke_values_blocking::<
                        Box<[Option<usize>]>,
                        (),
                        (Option<wrpc_transport::ResourceOwn<TestResource>>,),
                    >((), "transport:test", "resource-some", (), paths)
                    .await
                    .unwrap_or_else(|error| panic!("resource-some invocation failed: {error}"));
                let bytes =
                    value.unwrap_or_else(|| panic!("resource-some response must be present"));
                assert_eq!(
                    Bytes::from(bytes),
                    Bytes::from_static(&[0x12, 0x34, 0x56, 0x78])
                );

                server_task
                    .await
                    .unwrap_or_else(|error| panic!("resource-some server task panicked: {error}"));
            });
    }

    #[test]
    fn typed_zero_handle_resource_result_completes() {
        #[repr(transparent)]
        struct TestResource(());

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

                let invocations = server
                    .serve_values::<(), (Option<wrpc_transport::ResourceOwn<TestResource>>,)>(
                        "transport:test",
                        "resource-zero",
                        [],
                    )
                    .await
                    .unwrap_or_else(|error| {
                        panic!("failed to register resource-zero server handler: {error}")
                    });
                let mut invocations = Box::pin(invocations);
                let server_task = tokio::spawn(async move {
                    let (_, (), _rx, tx) = invocations
                        .as_mut()
                        .next()
                        .await
                        .unwrap_or_else(|| panic!("resource-zero invocation stream ended early"))
                        .unwrap_or_else(|error| {
                            panic!("failed to accept resource-zero invocation: {error}")
                        });
                    let handle =
                        wrpc_transport::ResourceOwn::from(Bytes::from_static(&[0, 0, 0, 0]));
                    tx((Some(handle),)).await.unwrap_or_else(|error| {
                        panic!("failed to send resource-zero response: {error}")
                    });
                });

                let paths: [Box<[Option<usize>]>; 0] = [];
                let (value,) = client
                    .invoke_values_blocking::<
                        Box<[Option<usize>]>,
                        (),
                        (Option<wrpc_transport::ResourceOwn<TestResource>>,),
                    >((), "transport:test", "resource-zero", (), paths)
                    .await
                    .unwrap_or_else(|error| panic!("resource-zero invocation failed: {error}"));
                let bytes =
                    value.unwrap_or_else(|| panic!("resource-zero response must be present"));
                assert_eq!(Bytes::from(bytes), Bytes::from_static(&[0, 0, 0, 0]));

                server_task
                    .await
                    .unwrap_or_else(|error| panic!("resource-zero server task panicked: {error}"));
            });
    }
}
