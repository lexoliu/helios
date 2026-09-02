use super::*;

pub(super) struct SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    pub(super) stream: OutputStreamKind,
    pub(super) result: Option<oneshot::Sender<core::result::Result<(), cli_types::ErrorCode>>>,
    /// Batch that the sink could not take yet. Held here — never dropped
    /// — until the child channel behind this stream has room again.
    pub(super) pending: Option<Bytes>,
    pub(super) write_wait: Option<crate::ByteWriteWait>,
}

impl<T, CpuImpl, HostFs> Unpin for SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<T, CpuImpl, HostFs> SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) fn new(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        result: oneshot::Sender<core::result::Result<(), cli_types::ErrorCode>>,
        stream: OutputStreamKind,
    ) -> Self {
        Self {
            getter,
            stream,
            result: Some(result),
            pending: None,
            write_wait: None,
        }
    }

    pub(super) fn complete(&mut self, result: core::result::Result<(), cli_types::ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(result);
        }
    }
}

impl<T, CpuImpl, HostFs> Drop for SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static, CpuImpl, HostFs> StreamConsumer<T> for SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = u8;

    /// Copy the guest's batch into the store's stdout/stderr sink.
    ///
    /// A child-pipe sink is bounded, so this parks — `pending` keeps the
    /// bytes and `poll_write_output_bytes` registers the waker — instead
    /// of dropping the batch or reporting a spurious error.
    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        if self.pending.is_none() {
            let available = source.remaining(&mut store);
            if available == 0 {
                return Poll::Ready(Ok(StreamResult::Completed));
            }
            let mut bytes = Vec::with_capacity(available);
            source.read(&mut store, &mut bytes)?;
            self.pending = Some(Bytes::from(bytes));
        }

        let consumer = &mut *self;
        let getter = consumer.getter;
        let stream = consumer.stream;
        match getter(store.data_mut()).poll_write_output_bytes(
            stream,
            cx,
            &mut consumer.write_wait,
            &mut consumer.pending,
        ) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => Poll::Ready(Ok(StreamResult::Completed)),
        }
    }
}

#[derive(Default)]
pub struct BytesStreamBuffer {
    pub(super) bytes: Bytes,
    pub(super) offset: usize,
}

impl BytesStreamBuffer {
    pub(super) fn new(bytes: Bytes) -> Self {
        Self { bytes, offset: 0 }
    }
}

unsafe impl WriteBuffer<u8> for BytesStreamBuffer {
    fn remaining(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }

    fn skip(&mut self, count: usize) {
        assert!(count <= self.remaining().len());
        self.offset += count;
    }

    fn take(&mut self, count: usize, fun: &mut dyn FnMut(&[MaybeUninit<u8>])) {
        assert!(count <= self.remaining().len());
        let slice = &self.remaining()[..count];
        // SAFETY: `u8` has no invalid bit patterns and the input slice is
        // fully initialized for every byte Wasmtime is allowed to take.
        fun(unsafe { core::mem::transmute::<&[u8], &[MaybeUninit<u8>]>(slice) });
        self.skip(count);
    }
}

/// Bridges a kernel [`ByteReader`](crate::ByteReader) to a wasmtime
/// component stream producer. Used for both `wasi:cli/stdin.read-via-stream`
/// (when spawn-mode hooks the child's stdin to the parent channel) and
/// `child.stdout` / `child.stderr` on the parent side.
///
/// Because `ByteReader::read` is async and `poll_produce` is sync, we
/// keep a pinned boxed future representing the in-flight read; every
/// poll drives it until a chunk is produced.
pub(crate) struct ChannelStreamProducer {
    pub(super) reader: crate::ByteReader,
    pub(super) read_wait: crate::ByteReadWait,
    pub(super) completion: Option<oneshot::Sender<()>>,
}

impl ChannelStreamProducer {
    pub(crate) fn new(reader: crate::ByteReader) -> Self {
        let read_wait = reader.wait_state();
        Self {
            reader,
            read_wait,
            completion: None,
        }
    }

    pub(crate) fn new_with_completion(
        reader: crate::ByteReader,
        completion: oneshot::Sender<()>,
    ) -> Self {
        let read_wait = reader.wait_state();
        Self {
            reader,
            read_wait,
            completion: Some(completion),
        }
    }

    pub(super) fn finish(&mut self) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(());
        }
    }
}

impl Drop for ChannelStreamProducer {
    fn drop(&mut self) {
        self.finish();
    }
}

impl<T> StreamProducer<T> for ChannelStreamProducer {
    type Item = u8;
    type Buffer = BytesStreamBuffer;

    fn poll_produce(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _: wasmtime::StoreContextMut<'_, T>,
        mut destination: Destination<'_, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        if finish {
            self.finish();
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        loop {
            let reader = self.reader.clone();
            match reader.poll_read(cx, &mut self.read_wait) {
                Poll::Pending => {
                    return Poll::Pending;
                }
                Poll::Ready(None) => {
                    self.finish();
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                Poll::Ready(Some(bytes)) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    destination.set_buffer(BytesStreamBuffer::new(bytes));
                    return Poll::Ready(Ok(StreamResult::Completed));
                }
            }
        }
    }
}

/// Bridges a wasmtime component stream consumer to a kernel
/// [`ByteWriter`](crate::ByteWriter). Used for `child.stdin` so the
/// parent-supplied stream is copied into the child's stdin channel.
pub(crate) struct ChannelStreamConsumer {
    pub(super) writer: crate::ByteWriter,
    pub(super) completion: Option<oneshot::Sender<core::result::Result<(), ()>>>,
    /// Batch the channel could not take yet, kept until it fits.
    pub(super) pending: Option<Bytes>,
    pub(super) write_wait: Option<crate::ByteWriteWait>,
}

impl Unpin for ChannelStreamConsumer {}

impl ChannelStreamConsumer {
    pub(crate) fn new(
        writer: crate::ByteWriter,
        completion: oneshot::Sender<core::result::Result<(), ()>>,
    ) -> Self {
        Self {
            writer,
            completion: Some(completion),
            pending: None,
            write_wait: None,
        }
    }

    /// A consumer with no completion signal, for callers that only care that
    /// the channel closes once the stream ends.
    ///
    /// Dropping the consumer drops `writer`, and the last writer handle going
    /// away is what publishes end-of-stream to the reader — which is the whole
    /// signal an HTTP body needs.
    pub(crate) fn detached(writer: crate::ByteWriter) -> Self {
        Self {
            writer,
            completion: None,
            pending: None,
            write_wait: None,
        }
    }

    pub(super) fn finish(&mut self, result: core::result::Result<(), ()>) {
        if let Some(tx) = self.completion.take() {
            let _ = tx.send(result);
        }
    }
}

impl Drop for ChannelStreamConsumer {
    fn drop(&mut self) {
        self.finish(Ok(()));
    }
}

impl<T: 'static> StreamConsumer<T> for ChannelStreamConsumer {
    type Item = u8;

    /// Copy the parent's batch into the child's stdin channel, parking on
    /// a full channel the way [`TcpWriteConsumer`](super::net::TcpWriteConsumer)
    /// parks on a busy socket.
    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        if self.pending.is_none() {
            let available = source.remaining(&mut store);
            if available == 0 {
                return Poll::Ready(Ok(StreamResult::Completed));
            }
            let mut bytes = Vec::with_capacity(available);
            source.read(&mut store, &mut bytes)?;
            self.pending = Some(Bytes::from(bytes));
        }

        let consumer = &mut *self;
        let wait = consumer
            .write_wait
            .get_or_insert_with(|| consumer.writer.wait_state());
        match consumer.writer.poll_write(cx, wait, &mut consumer.pending) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => Poll::Ready(Ok(StreamResult::Completed)),
            Poll::Ready(Err(_closed)) => Poll::Ready(Ok(StreamResult::Dropped)),
        }
    }
}
