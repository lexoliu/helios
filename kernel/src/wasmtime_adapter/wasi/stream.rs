use super::*;

pub(super) struct SerialStreamConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    pub(super) stream: OutputStreamKind,
    pub(super) result: Option<oneshot::Sender<core::result::Result<(), cli_types::ErrorCode>>>,
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

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        let available = source.remaining(&mut store);
        if available == 0 {
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        let mut bytes = Vec::with_capacity(available);
        source.read(&mut store, &mut bytes)?;
        let consumer = self.as_ref().get_ref();
        let getter = consumer.getter;
        getter(store.data_mut()).write_output_bytes(consumer.stream, Bytes::from(bytes));
        Poll::Ready(Ok(StreamResult::Completed))
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
}

impl ChannelStreamConsumer {
    pub(crate) fn new(
        writer: crate::ByteWriter,
        completion: oneshot::Sender<core::result::Result<(), ()>>,
    ) -> Self {
        Self {
            writer,
            completion: Some(completion),
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

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        let available = source.remaining(&mut store);
        if available == 0 {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let mut bytes = Vec::with_capacity(available);
        source.read(&mut store, &mut bytes)?;
        match self.as_ref().get_ref().writer.write(bytes) {
            Ok(()) => Poll::Ready(Ok(StreamResult::Completed)),
            Err(_closed) => Poll::Ready(Ok(StreamResult::Dropped)),
        }
    }
}
