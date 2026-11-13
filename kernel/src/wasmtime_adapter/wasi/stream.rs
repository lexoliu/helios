use super::*;
use crate::ComponentHostNetwork;

/// How many bytes one poll of a stream consumer takes from a write of
/// `remaining` bytes: the whole write when it fits `limit`, `limit`
/// otherwise.
pub(super) const fn bounded_chunk_len(remaining: usize, limit: usize) -> usize {
    if remaining < limit { remaining } else { limit }
}

/// Takes at most `limit` bytes of the guest's pending write into an owned
/// buffer, empty when the write is. The reservation is the chunk, never the
/// write, so what a consumer holds does not depend on the guest's length;
/// every consumer that copies a write out of guest memory reads through
/// this and names the bound of its own sink.
///
/// The writer is told how many bytes were taken (`Source::read` advances
/// the write by what it moved), and Wasmtime does not poll again for the
/// rest: the guest's `stream.write` completes with that short count.
pub(super) fn read_bounded_chunk<T: 'static>(
    source: &mut Source<'_, u8>,
    store: &mut StoreContextMut<'_, T>,
    limit: usize,
) -> Result<Vec<u8>> {
    let len = bounded_chunk_len(source.remaining(&mut *store), limit);
    let mut bytes = Vec::with_capacity(len);
    if len != 0 {
        source.read(&mut *store, &mut bytes)?;
    }
    Ok(bytes)
}

pub(super) struct SerialStreamConsumer<T, CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    pub(super) getter: fn(&mut T) -> &mut StoreData<CpuImpl, Net, StorageImpl>,
    pub(super) stream: OutputStreamKind,
    pub(super) result: Option<oneshot::Sender<core::result::Result<(), cli_types::ErrorCode>>>,
    /// Batch that the sink could not take yet. Held here — never dropped
    /// — until the child channel behind this stream has room again.
    pub(super) pending: Option<Bytes>,
    pub(super) write_wait: Option<crate::ByteWriteWait>,
    /// The debug console's transmit wait, kept armed across polls so a
    /// serial sink parked on a busy port is woken by its release.
    pub(super) serial_wait: Option<crate::NotifyWaiter>,
}

impl<T, CpuImpl, Net, StorageImpl> Unpin for SerialStreamConsumer<T, CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
}

impl<T, CpuImpl, Net, StorageImpl> SerialStreamConsumer<T, CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    pub(super) fn new(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, Net, StorageImpl>,
        result: oneshot::Sender<core::result::Result<(), cli_types::ErrorCode>>,
        stream: OutputStreamKind,
    ) -> Self {
        Self {
            getter,
            stream,
            result: Some(result),
            pending: None,
            write_wait: None,
            serial_wait: None,
        }
    }

    pub(super) fn complete(&mut self, result: core::result::Result<(), cli_types::ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(result);
        }
    }
}

impl<T, CpuImpl, Net, StorageImpl> Drop for SerialStreamConsumer<T, CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static, CpuImpl, Net, StorageImpl> StreamConsumer<T>
    for SerialStreamConsumer<T, CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    type Item = u8;

    /// Copy one chunk of the guest's batch into the store's stdout/stderr
    /// sink.
    ///
    /// The chunk is [`GUEST_IO_CHUNK_BYTES`] at most and the guest's
    /// `stream.write` returns the count taken, so the rest of a larger
    /// write comes back as the guest's next write. The console segments
    /// what it is offered at its own bound, so a chunk costs one pass
    /// through that loop, and `pending` never holds more than one chunk.
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
            let bytes = read_bounded_chunk(&mut source, &mut store, GUEST_IO_CHUNK_BYTES)?;
            if bytes.is_empty() {
                return Poll::Ready(Ok(StreamResult::Completed));
            }
            self.pending = Some(Bytes::from(bytes));
        }

        let consumer = &mut *self;
        let getter = consumer.getter;
        let stream = consumer.stream;
        match getter(store.data_mut()).poll_write_output_bytes(
            stream,
            cx,
            &mut consumer.write_wait,
            &mut consumer.serial_wait,
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

    /// Copy one chunk of the parent's batch into the child's stdin channel,
    /// parking on a full channel the way
    /// [`TcpWriteConsumer`](super::net::TcpWriteConsumer) parks on a busy
    /// socket.
    ///
    /// The channel bounds how many chunks it queues, not their size, so a
    /// chunk is [`GUEST_IO_CHUNK_BYTES`] at most and what the channel
    /// holds is that many bytes per slot. The guest's `stream.write`
    /// returns the count taken and the rest is its next write.
    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        if self.pending.is_none() {
            let bytes = read_bounded_chunk(&mut source, &mut store, GUEST_IO_CHUNK_BYTES)?;
            if bytes.is_empty() {
                return Poll::Ready(Ok(StreamResult::Completed));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcessAuthority;
    use crate::wasmtime_adapter::component_host::service::test_store::{
        TestStoreData, store_data_routed,
    };
    use futures_lite::future::block_on;
    use wasmtime::component::StreamReader;

    /// A guest write several chunks long whose last chunk is short, with
    /// bytes that differ from one chunk to the next so a reordered or
    /// repeated chunk shows.
    fn long_guest_write() -> Vec<u8> {
        (0..3 * GUEST_IO_CHUNK_BYTES + 17)
            .map(|index| (index % 251) as u8)
            .collect()
    }

    fn async_engine() -> wasmtime::Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        wasmtime::Engine::new(&config).expect("an engine with the component model async enabled")
    }

    /// Everything `reader` delivers until `total` bytes have arrived, as
    /// the chunks the sink was handed.
    async fn chunks_until(reader: &crate::ByteReader, total: usize) -> Vec<Bytes> {
        let mut chunks = Vec::new();
        let mut taken = 0;
        while taken < total {
            let chunk = reader
                .read()
                .await
                .expect("the sink delivers every byte before it closes");
            taken += chunk.len();
            chunks.push(chunk);
        }
        chunks
    }

    fn assert_arrived_in_bounded_chunks(chunks: &[Bytes], guest: &[u8]) {
        let lens: Vec<usize> = chunks.iter().map(Bytes::len).collect();
        assert_eq!(
            lens,
            [
                GUEST_IO_CHUNK_BYTES,
                GUEST_IO_CHUNK_BYTES,
                GUEST_IO_CHUNK_BYTES,
                17
            ],
            "each poll hands the sink one chunk, never the whole write"
        );
        let arrived: Vec<u8> = chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect();
        assert_eq!(arrived, guest, "every byte arrives, in order");
    }

    #[test]
    fn a_write_larger_than_any_heap_asks_for_one_chunk() {
        // `usize::MAX` is a write no heap could hold: the reservation is
        // the chunk, whatever the guest says.
        assert_eq!(
            bounded_chunk_len(usize::MAX, GUEST_IO_CHUNK_BYTES),
            GUEST_IO_CHUNK_BYTES
        );
        assert_eq!(
            bounded_chunk_len(GUEST_IO_CHUNK_BYTES + 1, GUEST_IO_CHUNK_BYTES),
            GUEST_IO_CHUNK_BYTES
        );
        assert_eq!(
            bounded_chunk_len(GUEST_IO_CHUNK_BYTES, GUEST_IO_CHUNK_BYTES),
            GUEST_IO_CHUNK_BYTES
        );
        assert_eq!(bounded_chunk_len(17, GUEST_IO_CHUNK_BYTES), 17);
        assert_eq!(bounded_chunk_len(0, GUEST_IO_CHUNK_BYTES), 0);
    }

    #[test]
    fn child_stdin_consumer_hands_the_channel_one_chunk_per_poll() {
        let engine = async_engine();
        let mut store = wasmtime::Store::new(&engine, ());
        let (writer, reader) = crate::byte_channel();
        let guest = long_guest_write();
        let total = guest.len();

        let chunks = block_on(store.run_concurrent(async move |accessor| {
            accessor.with(|mut access| {
                let stream = StreamReader::new(&mut access, guest.clone())
                    .expect("a host stream over the guest's bytes");
                stream
                    .pipe(&mut access, ChannelStreamConsumer::detached(writer))
                    .expect("the consumer takes the stream");
            });
            chunks_until(&reader, total).await
        }))
        .expect("the concurrent run completes");

        assert_arrived_in_bounded_chunks(&chunks, &long_guest_write());
    }

    #[test]
    fn stdout_consumer_hands_a_child_pipe_one_chunk_per_poll() {
        let engine = async_engine();
        let (stdout_tx, stdout_rx) = crate::byte_channel();
        let (stderr_tx, _stderr_rx) = crate::byte_channel();
        let (_stdin_tx, stdin_rx) = crate::byte_channel();
        let data: TestStoreData = store_data_routed(
            ProcessAuthority::empty(),
            crate::ComponentOutputMode::Child {
                stdin_rx,
                stdout_tx,
                stderr_tx,
            },
            |_| {},
        );
        let mut store = wasmtime::Store::new(&engine, data);
        let guest = long_guest_write();
        let total = guest.len();
        let (result_tx, _result_rx) = oneshot::channel();

        let chunks = block_on(store.run_concurrent(async move |accessor| {
            accessor.with(|mut access| {
                let stream = StreamReader::new(&mut access, guest.clone())
                    .expect("a host stream over the guest's bytes");
                let consumer = SerialStreamConsumer::new(
                    |data: &mut TestStoreData| data,
                    result_tx,
                    OutputStreamKind::Stdout,
                );
                stream
                    .pipe(&mut access, consumer)
                    .expect("the consumer takes the stream");
            });
            chunks_until(&stdout_rx, total).await
        }))
        .expect("the concurrent run completes");

        assert_arrived_in_bounded_chunks(&chunks, &long_guest_write());
    }
}
