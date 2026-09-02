//! The kernel's vsock stream sockets.
//!
//! [`table`] holds the protocol: the connection table, the handshake,
//! the credit arithmetic and the shutdown ordering, all synchronous and
//! device-free. This module is the part that touches the executor — it
//! owns the device, runs the receive pump as its own task, and turns
//! each table decision into a packet on the wire.
//!
//! Concurrency contract: the table lives behind a spin mutex that is
//! never held across an await; every operation takes it, decides, drops
//! it, and only then transmits. Waiters park on one shared
//! [`Notify`](crate::Notify) that the pump signals after each packet:
//! progress on a vsock link is always the arrival of a packet, and a
//! machine holds few enough connections that one broadcast per packet
//! costs less than a notification per connection would.

mod table;

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;

use helios_hal::cpu::{Cpu, Instant};
use helios_hal::vsock::{
    VsockAddress, VsockDelivery, VsockDevice, VsockPacketHeader, VsockShutdown,
};
use spin::Mutex as SpinMutex;
use triomphe::Arc;

use crate::{Notify, Timer};

pub use table::{
    MAX_VSOCK_BACKLOG, MAX_VSOCK_CONNECTIONS, MAX_VSOCK_LISTENERS, VSOCK_RECEIVE_WINDOW_BYTES,
    VsockError, VsockListenerId, VsockReadProgress, VsockStreamId, VsockTable, VsockWriteProgress,
};

/// The kernel's vsock service: one device, one connection table.
///
/// Generic over the device so no dynamic dispatch reaches the packet
/// path; the component host erases it once, at its own boundary, the
/// same way it does for the network service.
pub struct VsockService<CpuImpl, Device>
where
    CpuImpl: Cpu + Clone,
    Device: VsockDevice,
{
    inner: Arc<VsockServiceInner<CpuImpl, Device>>,
}

impl<CpuImpl, Device> Clone for VsockService<CpuImpl, Device>
where
    CpuImpl: Cpu + Clone,
    Device: VsockDevice,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct VsockServiceInner<CpuImpl, Device>
where
    CpuImpl: Cpu + Clone,
    Device: VsockDevice,
{
    cpu: CpuImpl,
    timer: Timer<CpuImpl>,
    device: Device,
    table: SpinMutex<VsockTable>,
    /// Signalled whenever a packet arrived, so every parked reader,
    /// writer and accepter re-tests what it was waiting for.
    progress: Notify,
}

impl<CpuImpl, Device> VsockService<CpuImpl, Device>
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    Device: VsockDevice + 'static,
{
    pub fn new(cpu: CpuImpl, timer: Timer<CpuImpl>, device: Device) -> Self {
        let table = VsockTable::new(device.guest_cid(), device.max_payload_bytes());
        Self {
            inner: Arc::new(VsockServiceInner {
                cpu,
                timer,
                device,
                table: SpinMutex::new(table),
                progress: Notify::new(),
            }),
        }
    }

    /// The context id the hypervisor assigned this machine.
    pub fn guest_cid(&self) -> u64 {
        self.inner.table.lock().guest_cid()
    }

    /// Drives the receive path for the lifetime of the machine.
    ///
    /// Every arriving packet is fed through the table and whatever the
    /// table decides to answer goes straight back out; a packet the
    /// device could not decode is dropped with a warning rather than
    /// stopping the pump, because the slot it arrived in has already
    /// gone back to the device and the next packet is unaffected.
    pub async fn run_forever(self) {
        let mut payload = vec![0_u8; self.inner.device.max_payload_bytes()].into_boxed_slice();
        tracing::info!(guest_cid = self.guest_cid(), "vsock receive pump started");
        loop {
            match self.inner.device.receive_into(&mut payload).await {
                Ok(VsockDelivery::Packet(received)) => {
                    // Connection lifecycle is worth a line each: the
                    // packets that carry it are a handful per session,
                    // and a link that never gets past its handshake is
                    // otherwise indistinguishable from one that is idle.
                    tracing::info!(
                        op = ?received.header.op,
                        source_cid = received.header.source.cid,
                        source_port = received.header.source.port,
                        destination_port = received.header.destination.port,
                        payload_len = received.payload_len,
                        "vsock packet received"
                    );
                    let reply = self
                        .inner
                        .table
                        .lock()
                        .handle_packet(&received.header, &payload[..received.payload_len]);
                    if let Some(reply) = reply
                        && let Err(error) = self.transmit(reply, &[]).await
                    {
                        tracing::warn!(?error, "vsock reply could not be transmitted");
                    }
                }
                Ok(VsockDelivery::TransportReset) => {
                    tracing::warn!("vsock transport was reset; every connection is gone");
                    self.inner.table.lock().reset_all();
                }
                Err(error) => {
                    tracing::warn!(?error, "vsock device delivered an unusable packet");
                }
            }
            self.inner.progress.notify_all();
        }
    }

    /// Binds `port`, or an ephemeral port when `port` is zero.
    pub fn listen(&self, port: u32, backlog: usize) -> Result<VsockListenerId, VsockError> {
        let listener = self.inner.table.lock().listen(port, backlog)?;
        tracing::debug!(
            port = self
                .inner
                .table
                .lock()
                .listener_port(listener)
                .unwrap_or(port),
            "vsock listener bound"
        );
        Ok(listener)
    }

    /// The port a listener is bound to, which a caller that asked for an
    /// ephemeral port needs in order to name it.
    pub fn listener_port(&self, listener: VsockListenerId) -> Result<u32, VsockError> {
        self.inner.table.lock().listener_port(listener)
    }

    pub fn close_listener(&self, listener: VsockListenerId) -> Result<(), VsockError> {
        self.inner.table.lock().close_listener(listener)
    }

    /// Waits for the next connection queued on `listener`.
    pub async fn accept(
        &self,
        listener: VsockListenerId,
        timeout_nanos: u64,
    ) -> Result<VsockStreamId, VsockError> {
        let deadline = self.deadline(timeout_nanos);
        loop {
            if let Some(stream) = self.inner.table.lock().accept(listener)? {
                return Ok(stream);
            }
            if self.expired(deadline) {
                return Err(VsockError::Timeout);
            }
            self.wait_for_progress(deadline).await;
        }
    }

    /// Opens a connection to `peer` and waits for the handshake.
    pub async fn connect(
        &self,
        peer: VsockAddress,
        timeout_nanos: u64,
    ) -> Result<VsockStreamId, VsockError> {
        let deadline = self.deadline(timeout_nanos);
        let (stream, request) = self.inner.table.lock().connect(peer)?;
        if let Err(error) = self.transmit(request, &[]).await {
            let _ = self.inner.table.lock().close(stream);
            return Err(error);
        }
        loop {
            match self.inner.table.lock().connect_progress(stream) {
                Ok(true) => return Ok(stream),
                Ok(false) => {}
                Err(error) => {
                    let _ = self.inner.table.lock().close(stream);
                    return Err(error);
                }
            }
            if self.expired(deadline) {
                let _ = self.close(stream).await;
                return Err(VsockError::Timeout);
            }
            self.wait_for_progress(deadline).await;
        }
    }

    pub fn peer(&self, stream: VsockStreamId) -> Result<VsockAddress, VsockError> {
        self.inner.table.lock().peer(stream)
    }

    /// Reads up to `max_bytes` from a connection.
    ///
    /// `None` is an orderly end of file: the peer closed its transmit
    /// side and everything it sent has been handed over.
    pub async fn read(
        &self,
        stream: VsockStreamId,
        max_bytes: usize,
        timeout_nanos: u64,
    ) -> Result<Option<Vec<u8>>, VsockError> {
        if max_bytes == 0 {
            return Ok(Some(Vec::new()));
        }
        let deadline = self.deadline(timeout_nanos);
        // A reader can never take more than the window holds, so the
        // buffer is bounded by it however much the caller asks for.
        let mut buffer = vec![0_u8; max_bytes.min(VSOCK_RECEIVE_WINDOW_BYTES)];
        loop {
            let progress = self.inner.table.lock().read(stream, &mut buffer)?;
            match progress {
                VsockReadProgress::Ready { len, credit_update } => {
                    if let Some(update) = credit_update {
                        self.transmit(update, &[]).await?;
                    }
                    buffer.truncate(len);
                    return Ok(Some(buffer));
                }
                VsockReadProgress::Eof => return Ok(None),
                VsockReadProgress::Blocked => {
                    if self.expired(deadline) {
                        return Err(VsockError::Timeout);
                    }
                    self.wait_for_progress(deadline).await;
                }
            }
        }
    }

    /// Writes as much of `bytes` as the deadline allows, returning how
    /// many bytes reached the device.
    ///
    /// A short return is backpressure, not an error: the peer's window
    /// is what bounds it, and the caller decides whether to come back
    /// for the rest.
    pub async fn write(
        &self,
        stream: VsockStreamId,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<usize, VsockError> {
        let deadline = self.deadline(timeout_nanos);
        let mut sent = 0;
        while sent < bytes.len() {
            let progress = self
                .inner
                .table
                .lock()
                .begin_write(stream, bytes.len() - sent)?;
            match progress {
                VsockWriteProgress::Ready(chunk) => {
                    let outcome = self
                        .transmit(chunk.header, &bytes[sent..sent + chunk.len])
                        .await;
                    match outcome {
                        Ok(()) => {
                            self.inner.table.lock().finish_write(stream, chunk.len);
                            sent += chunk.len;
                        }
                        Err(error) => {
                            self.inner.table.lock().finish_write(stream, 0);
                            return Err(error);
                        }
                    }
                }
                VsockWriteProgress::Blocked => {
                    if let Some(request) = self.inner.table.lock().credit_request(stream)? {
                        self.transmit(request, &[]).await?;
                    }
                    if self.expired(deadline) {
                        return if sent == 0 {
                            Err(VsockError::Timeout)
                        } else {
                            Ok(sent)
                        };
                    }
                    self.wait_for_progress(deadline).await;
                }
            }
        }
        Ok(sent)
    }

    /// Announces that this end closes the given directions.
    pub async fn shutdown(
        &self,
        stream: VsockStreamId,
        shutdown: VsockShutdown,
    ) -> Result<(), VsockError> {
        let announcement = self.inner.table.lock().shutdown(stream, shutdown)?;
        if let Some(announcement) = announcement {
            self.transmit(announcement, &[]).await?;
        }
        Ok(())
    }

    /// Drops a connection and tells the peer it is gone.
    pub async fn close(&self, stream: VsockStreamId) -> Result<(), VsockError> {
        let reset = self.inner.table.lock().close(stream)?;
        if let Some(reset) = reset {
            self.transmit(reset, &[]).await?;
        }
        Ok(())
    }

    async fn transmit(&self, header: VsockPacketHeader, payload: &[u8]) -> Result<(), VsockError> {
        tracing::info!(
            op = ?header.op,
            source_port = header.source.port,
            destination_cid = header.destination.cid,
            destination_port = header.destination.port,
            payload_len = payload.len(),
            "vsock packet transmitted"
        );
        self.inner
            .device
            .send(header, payload)
            .await
            .map_err(VsockError::Device)
    }

    /// Turns a caller's timeout into an absolute deadline on this
    /// processor's clock.
    ///
    /// The conversion saturates rather than wrapping: a caller that asks
    /// for `u64::MAX` nanoseconds means "no deadline", and a truncated
    /// tick count would turn that into an immediate timeout.
    fn deadline(&self, timeout_nanos: u64) -> Instant {
        let frequency = self.inner.cpu.timer_frequency();
        let ticks = u128::from(timeout_nanos) * u128::from(frequency) / 1_000_000_000;
        self.inner
            .cpu
            .now()
            .saturating_add(u64::try_from(ticks).unwrap_or(u64::MAX))
    }

    fn expired(&self, deadline: Instant) -> bool {
        self.inner.cpu.now() >= deadline
    }

    /// Parks until a packet arrives or the deadline passes.
    async fn wait_for_progress(&self, deadline: Instant) {
        let notified = self.inner.progress.notified();
        let mut notified = pin!(notified);
        let sleep = self.inner.timer.sleep_until(deadline);
        let mut sleep = pin!(sleep);
        poll_fn(|context| {
            if notified.as_mut().poll(context).is_ready() {
                return Poll::Ready(());
            }
            if sleep.as_mut().poll(context).is_ready() {
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await;
    }
}

/// Publishes a vsock device as the kernel's vsock service and starts its
/// receive pump.
pub fn install_vsock_device<CpuImpl, WatchdogImpl, Device>(
    kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    device: Device,
) -> VsockService<CpuImpl, Device>
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
    Device: VsockDevice + 'static,
{
    let service = VsockService::new(cpu.clone(), kernel.timer(), device);
    let pump = service.clone();
    kernel.spawn_local_detached(async move {
        pump.run_forever().await;
    });
    service
}

/// A boxed-future view of a [`VsockService`].
///
/// The component host's store type names one vsock service for every
/// backend, and the concrete device type differs per platform. This is
/// the same boundary — and the same reason — as the block and network
/// services draw: erase once, here, rather than making every consumer of
/// [`crate::RuntimeState`] generic over a device it never names.
trait DynVsockService: Send + Sync {
    fn guest_cid(&self) -> u64;

    fn listen(&self, port: u32, backlog: usize) -> Result<VsockListenerId, VsockError>;

    fn listener_port(&self, listener: VsockListenerId) -> Result<u32, VsockError>;

    fn close_listener(&self, listener: VsockListenerId) -> Result<(), VsockError>;

    fn accept(
        &self,
        listener: VsockListenerId,
        timeout_nanos: u64,
    ) -> BoxFuture<'_, Result<VsockStreamId, VsockError>>;

    fn connect(
        &self,
        peer: VsockAddress,
        timeout_nanos: u64,
    ) -> BoxFuture<'_, Result<VsockStreamId, VsockError>>;

    fn peer(&self, stream: VsockStreamId) -> Result<VsockAddress, VsockError>;

    fn read(
        &self,
        stream: VsockStreamId,
        max_bytes: usize,
        timeout_nanos: u64,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, VsockError>>;

    fn write<'a>(
        &'a self,
        stream: VsockStreamId,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> BoxFuture<'a, Result<usize, VsockError>>;

    fn shutdown(
        &self,
        stream: VsockStreamId,
        shutdown: VsockShutdown,
    ) -> BoxFuture<'_, Result<(), VsockError>>;

    fn close(&self, stream: VsockStreamId) -> BoxFuture<'_, Result<(), VsockError>>;
}

type BoxFuture<'a, T> = core::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl<CpuImpl, Device> DynVsockService for VsockService<CpuImpl, Device>
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    Device: VsockDevice + 'static,
{
    fn guest_cid(&self) -> u64 {
        VsockService::guest_cid(self)
    }

    fn listen(&self, port: u32, backlog: usize) -> Result<VsockListenerId, VsockError> {
        VsockService::listen(self, port, backlog)
    }

    fn listener_port(&self, listener: VsockListenerId) -> Result<u32, VsockError> {
        VsockService::listener_port(self, listener)
    }

    fn close_listener(&self, listener: VsockListenerId) -> Result<(), VsockError> {
        VsockService::close_listener(self, listener)
    }

    fn accept(
        &self,
        listener: VsockListenerId,
        timeout_nanos: u64,
    ) -> BoxFuture<'_, Result<VsockStreamId, VsockError>> {
        Box::pin(VsockService::accept(self, listener, timeout_nanos))
    }

    fn connect(
        &self,
        peer: VsockAddress,
        timeout_nanos: u64,
    ) -> BoxFuture<'_, Result<VsockStreamId, VsockError>> {
        Box::pin(VsockService::connect(self, peer, timeout_nanos))
    }

    fn peer(&self, stream: VsockStreamId) -> Result<VsockAddress, VsockError> {
        VsockService::peer(self, stream)
    }

    fn read(
        &self,
        stream: VsockStreamId,
        max_bytes: usize,
        timeout_nanos: u64,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, VsockError>> {
        Box::pin(VsockService::read(self, stream, max_bytes, timeout_nanos))
    }

    fn write<'a>(
        &'a self,
        stream: VsockStreamId,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> BoxFuture<'a, Result<usize, VsockError>> {
        Box::pin(VsockService::write(self, stream, bytes, timeout_nanos))
    }

    fn shutdown(
        &self,
        stream: VsockStreamId,
        shutdown: VsockShutdown,
    ) -> BoxFuture<'_, Result<(), VsockError>> {
        Box::pin(VsockService::shutdown(self, stream, shutdown))
    }

    fn close(&self, stream: VsockStreamId) -> BoxFuture<'_, Result<(), VsockError>> {
        Box::pin(VsockService::close(self, stream))
    }
}

/// The vsock service the component host serves `helios:system/vsock`
/// from, with the platform's device type erased.
#[derive(Clone)]
pub struct ComponentHostVsockService {
    inner: alloc::sync::Arc<dyn DynVsockService>,
}

impl ComponentHostVsockService {
    pub fn from_service<CpuImpl, Device>(service: VsockService<CpuImpl, Device>) -> Self
    where
        CpuImpl: Cpu + Clone + Send + Sync + 'static,
        Device: VsockDevice + 'static,
    {
        Self {
            inner: alloc::sync::Arc::new(service),
        }
    }

    pub fn guest_cid(&self) -> u64 {
        self.inner.guest_cid()
    }

    pub fn listen(&self, port: u32, backlog: usize) -> Result<VsockListenerId, VsockError> {
        self.inner.listen(port, backlog)
    }

    pub fn listener_port(&self, listener: VsockListenerId) -> Result<u32, VsockError> {
        self.inner.listener_port(listener)
    }

    pub fn close_listener(&self, listener: VsockListenerId) -> Result<(), VsockError> {
        self.inner.close_listener(listener)
    }

    pub fn accept(
        &self,
        listener: VsockListenerId,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<VsockStreamId, VsockError>> + Send + '_ {
        self.inner.accept(listener, timeout_nanos)
    }

    pub fn connect(
        &self,
        peer: VsockAddress,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<VsockStreamId, VsockError>> + Send + '_ {
        self.inner.connect(peer, timeout_nanos)
    }

    pub fn peer(&self, stream: VsockStreamId) -> Result<VsockAddress, VsockError> {
        self.inner.peer(stream)
    }

    pub fn read(
        &self,
        stream: VsockStreamId,
        max_bytes: usize,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, VsockError>> + Send + '_ {
        self.inner.read(stream, max_bytes, timeout_nanos)
    }

    pub fn write<'a>(
        &'a self,
        stream: VsockStreamId,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<usize, VsockError>> + Send + 'a {
        self.inner.write(stream, bytes, timeout_nanos)
    }

    pub fn shutdown(
        &self,
        stream: VsockStreamId,
        shutdown: VsockShutdown,
    ) -> impl Future<Output = Result<(), VsockError>> + Send + '_ {
        self.inner.shutdown(stream, shutdown)
    }

    pub fn close(
        &self,
        stream: VsockStreamId,
    ) -> impl Future<Output = Result<(), VsockError>> + Send + '_ {
        self.inner.close(stream)
    }
}
