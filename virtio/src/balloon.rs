//! virtio-balloon driver (virtio 1.2 §5.5).
//!
//! The device owns two directions of memory movement and two purely
//! advisory ones:
//!
//! * *inflate* hands guest frames to the host, which may reclaim them;
//!   *deflate* takes them back. The host publishes how many 4 KiB pages
//!   it wants the balloon to hold in `num_pages`, the driver reports how
//!   many it actually holds in `actual`.
//! * *free-page reporting* names runs of memory the guest currently has
//!   no use for, without giving them up. The host may drop their
//!   contents; the guest reads them back as zeroes.
//! * *free-page hinting* is the same information gathered on the host's
//!   command during migration, framed by a command identifier the device
//!   publishes in its configuration space.
//! * the *stats* queue lets the host ask for the guest's own view of its
//!   memory, one request at a time.
//!
//! Addresses: the balloon protocol counts in 4 KiB pages regardless of
//! the guest's page size, and every address the driver publishes is a
//! device address. Callers hand this driver the memory they own as
//! ordinary mutable slices, exactly as they would any other virtio
//! buffer, and the bus DMA pool performs the translation — so a backend
//! that maps physical memory at an offset needs no balloon-specific
//! address handling.
//!
//! Concurrency contract: every queue carries its own lock and completion
//! table, so the inflate/deflate path, the reporting task and a stats
//! request never serialize against each other. Configuration reads are
//! register reads and take no lock; the configuration-change interrupt
//! wakes [`VirtioBalloonDevice::config_changed`] waiters.

use core::future::Future;
use core::sync::atomic::{AtomicU32, Ordering};

use async_lock::Mutex;
use helios_hal::balloon::{MemoryBalloon, MemoryStat, MemoryStatTag};
use helios_hal::io::{IoError, IoResult};

use crate::bus::{DeviceBus, DmaPool};
use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate_with};
use crate::inflight::{InFlight, await_completion, submit_chain};
use crate::notify::Notify;
use crate::queue::{MAX_CHAIN_BUFFERS, VirtQueue};
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

/// Page size the balloon protocol counts in (virtio 1.2 §5.5.6). It is
/// fixed at 4 KiB and does not follow the guest's own page size.
pub const BALLOON_PAGE_SIZE: usize = 4096;
const BALLOON_PAGE_SHIFT: u32 = 12;

/// VIRTIO_BALLOON_F_MUST_TELL_HOST: the driver must tell the host before
/// it reuses a page it inflated.
const F_MUST_TELL_HOST: u64 = 1 << 0;
/// VIRTIO_BALLOON_F_STATS_VQ: the device carries a statistics queue.
const F_STATS_VQ: u64 = 1 << 1;
/// VIRTIO_BALLOON_F_DEFLATE_ON_OOM: the driver may deflate on memory
/// pressure without waiting for the host to lower its target.
const F_DEFLATE_ON_OOM: u64 = 1 << 2;
/// VIRTIO_BALLOON_F_FREE_PAGE_HINT: the device asks for free-page hints
/// through a command identifier in its configuration space.
const F_FREE_PAGE_HINT: u64 = 1 << 3;
/// VIRTIO_BALLOON_F_PAGE_REPORTING: the device accepts unsolicited
/// reports of free memory.
const F_PAGE_REPORTING: u64 = 1 << 5;

/// The device-class features this driver asks for when they are offered.
///
/// VIRTIO_BALLOON_F_PAGE_POISON is deliberately absent: helios does not
/// poison free memory, and claiming the feature would promise the host a
/// pattern it could then rely on finding.
const BALLOON_FEATURES: u64 =
    F_MUST_TELL_HOST | F_STATS_VQ | F_DEFLATE_ON_OOM | F_FREE_PAGE_HINT | F_PAGE_REPORTING;

/// `struct virtio_balloon_config` field offsets.
const CONFIG_NUM_PAGES: usize = 0;
const CONFIG_ACTUAL: usize = 4;
const CONFIG_FREE_PAGE_HINT_CMD_ID: usize = 8;

/// Reserved free-page-hint command identifiers (virtio 1.2 §5.5.6.3).
///
/// `STOP` ends a hint sequence, `DONE` is what the device leaves in the
/// configuration space once it has consumed one; neither starts a new
/// one.
pub const FREE_PAGE_CMD_ID_STOP: u32 = 0;
pub const FREE_PAGE_CMD_ID_DONE: u32 = 1;

const INFLATE_QUEUE_INDEX: u16 = 0;
const DEFLATE_QUEUE_INDEX: u16 = 1;

/// Queue depth every balloon queue is programmed with.
///
/// The balloon is driven by a single kernel task that keeps at most a
/// handful of requests outstanding, so a deep ring would only cost
/// descriptor memory.
const QUEUE_SIZE: u16 = 16;

/// Page-frame numbers one inflate or deflate request carries.
///
/// The array is built on the caller's stack for the duration of the
/// request, so it is bounded rather than sized after the range the
/// caller passed.
const PFNS_PER_REQUEST: usize = 256;

/// Statistics entries one stats reply carries.
const STATS_PER_REPLY: usize = 8;
/// Wire size of `struct virtio_balloon_stat`: a 16-bit tag followed by
/// an unaligned 64-bit value.
const STAT_BYTES: usize = 10;

/// The `VIRTIO_BALLOON_S_*` tag a published statistic goes out as.
///
/// The wire numbering is the device's, so it lives here rather than in
/// the contract the kernel writes its statistics against.
const fn stat_tag(tag: MemoryStatTag) -> u16 {
    match tag {
        MemoryStatTag::Free => 4,
        MemoryStatTag::Total => 5,
        MemoryStatTag::Available => 6,
    }
}

fn encode_stat(stat: MemoryStat, bytes: &mut [u8; STAT_BYTES]) {
    bytes[..2].copy_from_slice(&stat_tag(stat.tag).to_le_bytes());
    bytes[2..].copy_from_slice(&stat.value.to_le_bytes());
}

/// One virtqueue of a balloon device, with the completion table its
/// waiters are routed through and the notification they park on.
///
/// The notification is per queue rather than per device because a
/// balloon is driven by several independent tasks at once — one follows
/// the host's target, one reports free memory, one answers the
/// statistics queue — and a waiter only ever drains the queue it is
/// waiting on. A device-wide notification would let the task parked on
/// one queue consume the wake-up belonging to another, whose completion
/// would then sit in the used ring with nobody left to reap it.
struct BalloonQueue<T: VirtioTransport> {
    queue: Mutex<VirtQueue<T>>,
    inflight: InFlight<{ QUEUE_SIZE as usize }>,
    interrupts: Notify,
}

impl<T: VirtioTransport> BalloonQueue<T> {
    fn new(
        transport: &T,
        index: u16,
        chain_limit: u16,
        features: NegotiatedFeatures,
    ) -> IoResult<Self> {
        let size = transport.queue_max_size(index).min(QUEUE_SIZE);
        if size == 0 || !size.is_power_of_two() {
            return Err(IoError::Unsupported);
        }
        Ok(Self {
            queue: Mutex::new(VirtQueue::new(
                transport,
                index,
                size,
                chain_limit,
                features,
            )?),
            inflight: InFlight::new(),
            interrupts: Notify::new(),
        })
    }

    /// Places one chain and waits for the device to finish it.
    async fn request(
        &self,
        transport: &T,
        inputs: &[&[u8]],
        outputs: &mut [&mut [u8]],
    ) -> IoResult<u32> {
        let token = submit_chain(&self.inflight, &self.queue, transport, inputs, outputs).await?;
        Ok(await_completion(&self.inflight, &self.queue, token, || {
            self.interrupts.notified()
        })
        .await)
    }

    /// Wakes whoever is waiting on this queue.
    fn wake(&self) {
        self.interrupts.notify_all();
    }

    fn shutdown(&mut self, transport: &T) {
        self.queue.get_mut().shutdown(transport);
    }
}

/// A virtio memory-balloon device.
pub struct VirtioBalloonDevice<T: VirtioTransport> {
    transport: T,
    inflate: BalloonQueue<T>,
    deflate: BalloonQueue<T>,
    stats: Option<BalloonQueue<T>>,
    free_page: Option<BalloonQueue<T>>,
    reporting: Option<BalloonQueue<T>>,
    /// Wakes tasks parked on a configuration change.
    config_changes: Notify,
    features: NegotiatedFeatures,
    /// The page count last written to the `actual` configuration field.
    actual_pages: AtomicU32,
}

impl<T: VirtioTransport> VirtioBalloonDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::MemoryBalloon {
            return Err(IoError::Unsupported);
        }

        // The queue layout depends on what the device offers, not on
        // what the driver accepts: the device created its queues before
        // negotiation and their indices follow its own feature word.
        let mut offered = 0_u64;
        let features = negotiate_with(&transport, |device_features| {
            offered = device_features;
            RING_FEATURES | BALLOON_FEATURES
        })?;
        let layout = QueueLayout::from_offered(offered);

        let inflate = BalloonQueue::new(&transport, INFLATE_QUEUE_INDEX, 1, features)?;
        let deflate = BalloonQueue::new(&transport, DEFLATE_QUEUE_INDEX, 1, features)?;
        let stats = layout
            .stats
            .map(|index| BalloonQueue::new(&transport, index, 1, features))
            .transpose()?;
        let free_page = layout
            .free_page
            .map(|index| BalloonQueue::new(&transport, index, MAX_CHAIN_BUFFERS as u16, features))
            .transpose()?;
        let reporting = layout
            .reporting
            .map(|index| BalloonQueue::new(&transport, index, MAX_CHAIN_BUFFERS as u16, features))
            .transpose()?;

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        let device = Self {
            transport,
            inflate,
            deflate,
            stats,
            free_page,
            reporting,
            config_changes: Notify::new(),
            features,
            actual_pages: AtomicU32::new(0),
        };
        tracing::info!(
            target_pages = device.target_pages(),
            must_tell_host = device.must_tell_host(),
            deflate_on_oom = device.deflates_on_oom(),
            stats_queue = device.stats.is_some(),
            free_page_hint = device.hints_free_pages(),
            page_reporting = device.reports_free_pages(),
            "virtio-balloon online"
        );
        Ok(device)
    }

    /// The feature set this device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    /// VIRTIO_BALLOON_F_MUST_TELL_HOST: an inflated page may only be
    /// reused after the host has been told through the deflate queue.
    pub fn must_tell_host(&self) -> bool {
        self.features.device(F_MUST_TELL_HOST)
    }

    /// VIRTIO_BALLOON_F_DEFLATE_ON_OOM: the driver may deflate under
    /// memory pressure without the host lowering its target first.
    pub fn deflates_on_oom(&self) -> bool {
        self.features.device(F_DEFLATE_ON_OOM)
    }

    /// Whether unsolicited free-page reports are accepted.
    pub fn reports_free_pages(&self) -> bool {
        self.features.device(F_PAGE_REPORTING) && self.reporting.is_some()
    }

    /// Whether the device asks for free-page hints by command id.
    pub fn hints_free_pages(&self) -> bool {
        self.features.device(F_FREE_PAGE_HINT) && self.free_page.is_some()
    }

    /// Whether the device carries a statistics queue.
    pub fn publishes_stats(&self) -> bool {
        self.features.device(F_STATS_VQ) && self.stats.is_some()
    }

    /// Interrupt handlers only acknowledge the device and wake waiters.
    pub fn handle_interrupt(&self) {
        let status = self.transport.ack_interrupt();
        if status.config_change {
            self.config_changes.notify_all();
        }
        if status.used_buffer {
            // The interrupt says only that the device used *a* buffer,
            // so every queue's waiters have to look.
            for queue in self.queues() {
                queue.wake();
            }
        }
    }

    /// Every queue this device programmed, in index order.
    fn queues(&self) -> impl Iterator<Item = &BalloonQueue<T>> {
        [Some(&self.inflate), Some(&self.deflate)]
            .into_iter()
            .chain([
                self.stats.as_ref(),
                self.free_page.as_ref(),
                self.reporting.as_ref(),
            ])
            .flatten()
    }

    /// Waits for the device to change its configuration space.
    pub async fn config_changed(&self) {
        self.config_changes.notified().await;
    }

    /// `num_pages`: how many 4 KiB pages the host wants the balloon to
    /// hold.
    pub fn target_pages(&self) -> u32 {
        self.transport.read_config_u32(CONFIG_NUM_PAGES)
    }

    /// `actual`: how many pages the driver last told the host it holds.
    pub fn actual_pages(&self) -> u32 {
        self.actual_pages.load(Ordering::Acquire)
    }

    /// Publishes how many pages the balloon actually holds.
    ///
    /// The host reads this field to learn how far the guest got towards
    /// the target it asked for, so it is written even — especially —
    /// when the driver stopped short of the target.
    pub fn set_actual(&self, pages: u32) {
        self.actual_pages.store(pages, Ordering::Release);
        self.transport.write_config_u32(CONFIG_ACTUAL, pages);
    }

    /// `free_page_hint_cmd_id`: the command the device wants hints for.
    ///
    /// [`FREE_PAGE_CMD_ID_STOP`] and [`FREE_PAGE_CMD_ID_DONE`] are not
    /// requests; any other value starts a hint sequence.
    pub fn free_page_hint_cmd_id(&self) -> Option<u32> {
        if !self.hints_free_pages() {
            return None;
        }
        match self.transport.read_config_u32(CONFIG_FREE_PAGE_HINT_CMD_ID) {
            FREE_PAGE_CMD_ID_STOP | FREE_PAGE_CMD_ID_DONE => None,
            cmd_id => Some(cmd_id),
        }
    }

    /// Hands `ranges` to the host, which may reclaim the memory behind
    /// them.
    ///
    /// The caller keeps ownership of the frames — the host gives them
    /// back on deflate — but must not read or write them until then, and
    /// must treat their previous contents as lost.
    pub async fn inflate(&self, ranges: &mut [&mut [u8]]) -> IoResult<()> {
        self.post_page_frame_numbers(&self.inflate, ranges).await
    }

    /// Takes `ranges` back from the host.
    ///
    /// After this returns the frames are the guest's to use again. Their
    /// contents are whatever the host left there, so a caller that hands
    /// them to anything but itself has to zero them first.
    pub async fn deflate(&self, ranges: &mut [&mut [u8]]) -> IoResult<()> {
        self.post_page_frame_numbers(&self.deflate, ranges).await
    }

    /// Tells the host that `ranges` are free.
    ///
    /// Unlike inflation this gives nothing up: the guest may allocate the
    /// memory again at any time. The host is free to drop the contents,
    /// so the pages read back as zeroes.
    pub async fn report_free(&self, ranges: &mut [&mut [u8]]) -> IoResult<()> {
        let Some(reporting) = &self.reporting else {
            return Err(IoError::Unsupported);
        };
        for batch in ranges.chunks_mut(MAX_CHAIN_BUFFERS) {
            reporting.request(&self.transport, &[], batch).await?;
        }
        Ok(())
    }

    /// Opens a free-page hint sequence with the identifier the device
    /// published.
    ///
    /// The sequence the device expects is the command identifier, then
    /// the free memory itself through
    /// [`VirtioBalloonDevice::hint_free_pages`], then
    /// [`VirtioBalloonDevice::end_free_page_hint`] to close it out. It
    /// is three calls rather than one because the caller has to hold
    /// each run of free memory out of its allocator only for as long as
    /// the device is looking at it.
    pub async fn begin_free_page_hint(&self, cmd_id: u32) -> IoResult<()> {
        let Some(free_page) = &self.free_page else {
            return Err(IoError::Unsupported);
        };
        let start = cmd_id.to_le_bytes();
        free_page
            .request(&self.transport, &[&start], &mut [])
            .await?;
        Ok(())
    }

    /// Names free memory inside an open hint sequence.
    pub async fn hint_free_pages(&self, ranges: &mut [&mut [u8]]) -> IoResult<()> {
        let Some(free_page) = &self.free_page else {
            return Err(IoError::Unsupported);
        };
        for batch in ranges.chunks_mut(MAX_CHAIN_BUFFERS) {
            free_page.request(&self.transport, &[], batch).await?;
        }
        Ok(())
    }

    /// Closes an open hint sequence.
    pub async fn end_free_page_hint(&self) -> IoResult<()> {
        let Some(free_page) = &self.free_page else {
            return Err(IoError::Unsupported);
        };
        let stop = FREE_PAGE_CMD_ID_STOP.to_le_bytes();
        free_page
            .request(&self.transport, &[&stop], &mut [])
            .await?;
        Ok(())
    }

    /// Publishes the guest's own view of its memory on the stats queue.
    ///
    /// The device consumes one buffer per request it makes, so the
    /// caller submits a fresh one each time the previous one comes back.
    pub async fn submit_stats(&self, stats: &[MemoryStat]) -> IoResult<()> {
        let Some(queue) = &self.stats else {
            return Err(IoError::Unsupported);
        };
        if stats.len() > STATS_PER_REPLY {
            return Err(IoError::OutOfBounds);
        }
        let mut encoded = [0_u8; STATS_PER_REPLY * STAT_BYTES];
        for (index, stat) in stats.iter().enumerate() {
            let start = index * STAT_BYTES;
            let entry: &mut [u8; STAT_BYTES] = (&mut encoded[start..start + STAT_BYTES])
                .try_into()
                .unwrap_or_else(|_| panic!("balloon statistics entry has a fixed width"));
            encode_stat(*stat, entry);
        }
        let payload = &encoded[..stats.len() * STAT_BYTES];
        if payload.is_empty() {
            return Err(IoError::OutOfBounds);
        }
        queue.request(&self.transport, &[payload], &mut []).await?;
        Ok(())
    }

    /// Splits `ranges` into page-frame numbers and posts them in
    /// bounded batches on `queue`.
    async fn post_page_frame_numbers(
        &self,
        queue: &BalloonQueue<T>,
        ranges: &mut [&mut [u8]],
    ) -> IoResult<()> {
        let dma = self.transport.bus().dma();
        let mut pfns = [0_u32; PFNS_PER_REQUEST];
        let mut filled = 0usize;
        for range in ranges.iter() {
            if !range.len().is_multiple_of(BALLOON_PAGE_SIZE) {
                return Err(IoError::OutOfBounds);
            }
            for page in range.chunks(BALLOON_PAGE_SIZE) {
                let address = dma.dma_addr(page.as_ptr())?;
                if !address.is_multiple_of(BALLOON_PAGE_SIZE as u64) {
                    return Err(IoError::OutOfBounds);
                }
                pfns[filled] = u32::try_from(address >> BALLOON_PAGE_SHIFT)
                    .map_err(|_| IoError::OutOfBounds)?;
                filled += 1;
                if filled == PFNS_PER_REQUEST {
                    self.post_batch(queue, &pfns[..filled]).await?;
                    filled = 0;
                }
            }
        }
        if filled != 0 {
            self.post_batch(queue, &pfns[..filled]).await?;
        }
        Ok(())
    }

    async fn post_batch(&self, queue: &BalloonQueue<T>, pfns: &[u32]) -> IoResult<()> {
        let mut bytes = [0_u8; PFNS_PER_REQUEST * 4];
        for (index, pfn) in pfns.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&pfn.to_le_bytes());
        }
        queue
            .request(&self.transport, &[&bytes[..pfns.len() * 4]], &mut [])
            .await?;
        Ok(())
    }
}

impl<T: VirtioTransport> MemoryBalloon for VirtioBalloonDevice<T> {
    fn target_pages(&self) -> u32 {
        Self::target_pages(self)
    }

    fn set_actual(&self, pages: u32) {
        Self::set_actual(self, pages);
    }

    fn must_tell_host(&self) -> bool {
        Self::must_tell_host(self)
    }

    fn deflates_on_oom(&self) -> bool {
        Self::deflates_on_oom(self)
    }

    fn reports_free_pages(&self) -> bool {
        Self::reports_free_pages(self)
    }

    fn publishes_stats(&self) -> bool {
        Self::publishes_stats(self)
    }

    fn free_page_hint_cmd_id(&self) -> Option<u32> {
        Self::free_page_hint_cmd_id(self)
    }

    fn config_changed(&self) -> impl Future<Output = ()> + Send + '_ {
        Self::config_changed(self)
    }

    fn inflate<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = IoResult<()>> + Send + 'a {
        Self::inflate(self, ranges)
    }

    fn deflate<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = IoResult<()>> + Send + 'a {
        Self::deflate(self, ranges)
    }

    fn report_free<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = IoResult<()>> + Send + 'a {
        Self::report_free(self, ranges)
    }

    fn begin_free_page_hint(&self, cmd_id: u32) -> impl Future<Output = IoResult<()>> + Send + '_ {
        Self::begin_free_page_hint(self, cmd_id)
    }

    fn hint_free_pages<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = IoResult<()>> + Send + 'a {
        Self::hint_free_pages(self, ranges)
    }

    fn end_free_page_hint(&self) -> impl Future<Output = IoResult<()>> + Send + '_ {
        Self::end_free_page_hint(self)
    }

    fn submit_stats<'a>(
        &'a self,
        stats: &'a [MemoryStat],
    ) -> impl Future<Output = IoResult<()>> + Send + 'a {
        Self::submit_stats(self, stats)
    }
}

impl<T: VirtioTransport> Drop for VirtioBalloonDevice<T> {
    fn drop(&mut self) {
        self.inflate.shutdown(&self.transport);
        self.deflate.shutdown(&self.transport);
        for queue in [&mut self.stats, &mut self.free_page, &mut self.reporting]
            .into_iter()
            .flatten()
        {
            queue.shutdown(&self.transport);
        }
    }
}

/// Which virtqueue index carries which balloon queue.
///
/// The optional queues only exist when the device offered the feature
/// that defines them, and each present queue shifts the ones after it
/// down, so the layout is derived from the offered feature word once and
/// then fixed for the device's lifetime (virtio 1.2 §5.5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueueLayout {
    stats: Option<u16>,
    free_page: Option<u16>,
    reporting: Option<u16>,
}

impl QueueLayout {
    fn from_offered(offered: u64) -> Self {
        let mut next = DEFLATE_QUEUE_INDEX + 1;
        let mut take = |present: bool| {
            present.then(|| {
                let index = next;
                next += 1;
                index
            })
        };
        Self {
            stats: take(offered & F_STATS_VQ != 0),
            free_page: take(offered & F_FREE_PAGE_HINT != 0),
            reporting: take(offered & F_PAGE_REPORTING != 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BALLOON_PAGE_SIZE, BalloonQueue, CONFIG_ACTUAL, CONFIG_FREE_PAGE_HINT_CMD_ID,
        CONFIG_NUM_PAGES, F_DEFLATE_ON_OOM, F_FREE_PAGE_HINT, F_MUST_TELL_HOST, F_PAGE_REPORTING,
        F_STATS_VQ, FREE_PAGE_CMD_ID_STOP, QueueLayout, VirtioBalloonDevice,
    };
    use crate::bus::DmaBuffer;
    use crate::testing::{FakeTransport, FakeTransportConfig, WindowDmaBuffer, WindowDmaPool};
    use crate::transport::{DeviceType, VirtioFeatures, VirtioTransport};
    use alloc::vec::Vec;
    use core::pin::pin;
    use futures_lite::future::{block_on, poll_once};
    use helios_hal::balloon::{MemoryStat, MemoryStatTag};

    /// ISR bit 0: the device used a buffer.
    const USED_BUFFER_INTERRUPT: u32 = 1;
    /// ISR bit 1: the device changed its configuration space.
    const CONFIG_CHANGE_INTERRUPT: u32 = 2;

    const ALL_BALLOON_FEATURES: u64 = VirtioFeatures::VERSION_1.bits()
        | F_MUST_TELL_HOST
        | F_STATS_VQ
        | F_DEFLATE_ON_OOM
        | F_FREE_PAGE_HINT
        | F_PAGE_REPORTING;

    type Device = VirtioBalloonDevice<FakeTransport<WindowDmaPool>>;

    /// The memory a test's rings and pages come from.
    ///
    /// The balloon protocol carries 32-bit page frame numbers, so the
    /// pages a test posts must translate to small device addresses
    /// wherever the host heap happens to sit; the window pool provides
    /// that for the rings and the pages alike.
    fn arena() -> WindowDmaPool {
        WindowDmaPool::new(64 * BALLOON_PAGE_SIZE)
    }

    fn device_with(offered: u64, dma: WindowDmaPool) -> Device {
        VirtioBalloonDevice::new(FakeTransport::with_dma(
            FakeTransportConfig {
                device_type: DeviceType::MemoryBalloon,
                offered_features: offered,
                queue_size: 8,
                supports_queue_reset: false,
                absent_queues: &[],
            },
            dma,
        ))
        .expect("balloon device should initialize")
    }

    fn device() -> Device {
        device_with(ALL_BALLOON_FEATURES, arena())
    }

    /// Page-aligned guest memory a test can hand to the driver.
    struct Pages(WindowDmaBuffer);

    impl Pages {
        fn new(dma: &WindowDmaPool, count: usize) -> Self {
            Self(dma.pages(count))
        }

        fn range(&mut self, count: usize) -> &mut [u8] {
            &mut self.0.as_mut_slice()[..count * BALLOON_PAGE_SIZE]
        }

        /// The address the device sees for the first page.
        fn device_base(&self) -> u64 {
            self.0.phys_addr()
        }
    }

    /// The descriptor identifier the next submission on `queue` takes.
    ///
    /// A completed identifier goes back to the ring's free pool, so the
    /// requests of one sequence do not land on ascending descriptors and
    /// a test that assumed they did would read an empty slot.
    fn next_token(queue: &BalloonQueue<FakeTransport<WindowDmaPool>>) -> u16 {
        queue
            .queue
            .try_lock()
            .expect("the queue is idle between requests")
            .next_free_descriptor()
    }

    /// Plays the device: finishes `token` on `queue` and raises the
    /// used-buffer interrupt.
    fn complete(device: &Device, queue_index: u16, token: u16) {
        let queue = match queue_index {
            0 => &device.inflate,
            1 => &device.deflate,
            2 => device.stats.as_ref().expect("stats queue"),
            3 => device.free_page.as_ref().expect("free-page queue"),
            4 => device.reporting.as_ref().expect("reporting queue"),
            index => panic!("balloon has no queue {index}"),
        };
        queue
            .queue
            .try_lock()
            .expect("a parked driver does not hold the queue lock")
            .device_complete(token, 0);
        device.transport.raise_interrupt(USED_BUFFER_INTERRUPT);
        device.handle_interrupt();
    }

    #[test]
    fn a_wrong_device_type_is_rejected() {
        let rejected = VirtioBalloonDevice::new(FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Block,
            ..FakeTransportConfig::default()
        }))
        .err();
        assert_eq!(rejected, Some(helios_hal::io::IoError::Unsupported));
    }

    /// Every optional queue the device does not offer shifts the ones
    /// after it down, so a driver that assumed fixed indices would
    /// program the wrong ring.
    #[test]
    fn optional_queues_take_the_indices_the_offered_features_leave_them() {
        assert_eq!(
            QueueLayout::from_offered(F_STATS_VQ | F_FREE_PAGE_HINT | F_PAGE_REPORTING),
            QueueLayout {
                stats: Some(2),
                free_page: Some(3),
                reporting: Some(4),
            }
        );
        assert_eq!(
            QueueLayout::from_offered(F_STATS_VQ | F_PAGE_REPORTING),
            QueueLayout {
                stats: Some(2),
                free_page: None,
                reporting: Some(3),
            }
        );
        assert_eq!(
            QueueLayout::from_offered(0),
            QueueLayout {
                stats: None,
                free_page: None,
                reporting: None,
            }
        );
    }

    #[test]
    fn a_device_offering_no_optional_features_still_inflates() {
        let dma = arena();
        let device = device_with(VirtioFeatures::VERSION_1.bits(), dma.clone());
        assert!(!device.must_tell_host());
        assert!(!device.reports_free_pages());
        assert!(!device.hints_free_pages());
        assert!(!device.publishes_stats());

        let mut pages = Pages::new(&dma, 1);
        let mut range = [pages.range(1)];
        let mut inflate = pin!(device.inflate(&mut range));
        assert!(block_on(poll_once(inflate.as_mut())).is_none());
        complete(&device, 0, 0);
        assert_eq!(block_on(poll_once(inflate.as_mut())), Some(Ok(())));
    }

    #[test]
    fn the_target_comes_from_the_configuration_space() {
        let device = device();
        device.transport.set_config_u32(CONFIG_NUM_PAGES, 4096);
        assert_eq!(device.target_pages(), 4096);
    }

    /// The host reads `actual` to see how far the guest got, so the
    /// driver has to write the field rather than only remember it.
    #[test]
    fn publishing_actual_writes_the_configuration_field() {
        let device = device();
        device.set_actual(512);
        assert_eq!(device.actual_pages(), 512);
        assert_eq!(device.transport.read_config_u32(CONFIG_ACTUAL), 512);
    }

    /// One request carries one page-frame number per 4 KiB page of the
    /// ranges it was given, in ascending address order.
    #[test]
    fn inflate_posts_one_page_frame_number_per_page() {
        let dma = arena();
        let device = device_with(ALL_BALLOON_FEATURES, dma.clone());
        let mut pages = Pages::new(&dma, 3);
        let base = pages.device_base();
        let range = pages.range(3);
        let mut ranges = [range];

        let mut inflate = pin!(device.inflate(&mut ranges));
        assert!(block_on(poll_once(inflate.as_mut())).is_none());

        let chain = device
            .inflate
            .queue
            .try_lock()
            .expect("the driver parked without the queue lock")
            .device_request(0);
        let pfns: Vec<u32> = chain
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().expect("four bytes")))
            .collect();
        assert_eq!(
            pfns,
            [
                (base >> 12) as u32,
                ((base + BALLOON_PAGE_SIZE as u64) >> 12) as u32,
                ((base + 2 * BALLOON_PAGE_SIZE as u64) >> 12) as u32,
            ]
        );

        complete(&device, 0, 0);
        assert_eq!(block_on(poll_once(inflate.as_mut())), Some(Ok(())));
    }

    /// Giving frames back is a different queue from handing them over,
    /// and a driver that confused the two would tell the host to reclaim
    /// memory the guest had just taken back.
    #[test]
    fn deflate_posts_on_the_deflate_queue() {
        let dma = arena();
        let device = device_with(ALL_BALLOON_FEATURES, dma.clone());
        let mut pages = Pages::new(&dma, 1);
        let base = pages.device_base();
        let mut ranges = [pages.range(1)];

        let idle_inflate = device
            .inflate
            .queue
            .try_lock()
            .expect("inflate queue is idle")
            .available_descriptors();

        let mut deflate = pin!(device.deflate(&mut ranges));
        assert!(block_on(poll_once(deflate.as_mut())).is_none());

        assert_eq!(
            device
                .inflate
                .queue
                .try_lock()
                .expect("inflate queue is idle")
                .available_descriptors(),
            idle_inflate,
            "the inflate queue must not carry a deflate request"
        );
        let request = device
            .deflate
            .queue
            .try_lock()
            .expect("the driver parked without the queue lock")
            .device_request(0);
        assert_eq!(request, ((base >> 12) as u32).to_le_bytes());

        complete(&device, 1, 0);
        assert_eq!(block_on(poll_once(deflate.as_mut())), Some(Ok(())));
    }

    /// A range that is not a whole number of balloon pages cannot be
    /// expressed as page-frame numbers at all.
    #[test]
    fn a_partial_page_range_is_rejected() {
        let device = device();
        let mut bytes = [0_u8; BALLOON_PAGE_SIZE + 1];
        let mut ranges = [&mut bytes[..]];
        assert_eq!(
            block_on(device.inflate(&mut ranges)),
            Err(helios_hal::io::IoError::OutOfBounds)
        );
    }

    /// Free-page reporting hands the device the memory itself as
    /// writable buffers, one descriptor per run.
    #[test]
    fn reporting_posts_each_run_as_a_writable_descriptor() {
        let dma = arena();
        let device = device_with(ALL_BALLOON_FEATURES, dma.clone());
        let mut first = Pages::new(&dma, 2);
        let mut second = Pages::new(&dma, 1);
        let first_base = first.device_base();
        let second_base = second.device_base();
        let mut ranges = [first.range(2), second.range(1)];

        let mut report = pin!(device.report_free(&mut ranges));
        assert!(block_on(poll_once(report.as_mut())).is_none());

        let chain = device
            .reporting
            .as_ref()
            .expect("reporting queue")
            .queue
            .try_lock()
            .expect("the driver parked without the queue lock")
            .device_chain(0);
        assert_eq!(
            chain,
            [
                (first_base, 2 * BALLOON_PAGE_SIZE as u32, true),
                (second_base, BALLOON_PAGE_SIZE as u32, true),
            ]
        );

        complete(&device, 4, 0);
        assert_eq!(block_on(poll_once(report.as_mut())), Some(Ok(())));
    }

    /// The hint sequence is framed: the command identifier the device
    /// published, the free memory, then the stop identifier.
    #[test]
    fn a_free_page_hint_is_framed_by_its_command_identifier() {
        let dma = arena();
        let device = device_with(ALL_BALLOON_FEATURES, dma.clone());
        device
            .transport
            .set_config_u32(CONFIG_FREE_PAGE_HINT_CMD_ID, 7);
        assert_eq!(device.free_page_hint_cmd_id(), Some(7));

        let free_page = device.free_page.as_ref().expect("free-page queue");
        let mut pages = Pages::new(&dma, 1);
        let page_base = pages.device_base();
        let mut ranges = [pages.range(1)];
        // The identifier the device published opens the sequence.
        let mut begin = pin!(device.begin_free_page_hint(7));
        let token = next_token(free_page);
        assert!(block_on(poll_once(begin.as_mut())).is_none());
        assert_eq!(
            free_page
                .queue
                .try_lock()
                .expect("idle queue")
                .device_request(token),
            7_u32.to_le_bytes()
        );
        complete(&device, 3, token);
        assert_eq!(block_on(poll_once(begin.as_mut())), Some(Ok(())));

        // Then the free memory itself, as writable buffers.
        let mut hint = pin!(device.hint_free_pages(&mut ranges));
        let token = next_token(free_page);
        assert!(block_on(poll_once(hint.as_mut())).is_none());
        assert_eq!(
            free_page
                .queue
                .try_lock()
                .expect("idle queue")
                .device_chain(token),
            [(page_base, BALLOON_PAGE_SIZE as u32, true)]
        );
        complete(&device, 3, token);
        assert_eq!(block_on(poll_once(hint.as_mut())), Some(Ok(())));

        // And the stop identifier closes it out.
        let mut end = pin!(device.end_free_page_hint());
        let token = next_token(free_page);
        assert!(block_on(poll_once(end.as_mut())).is_none());
        assert_eq!(
            free_page
                .queue
                .try_lock()
                .expect("idle queue")
                .device_request(token),
            FREE_PAGE_CMD_ID_STOP.to_le_bytes()
        );
        complete(&device, 3, token);
        assert_eq!(block_on(poll_once(end.as_mut())), Some(Ok(())));
    }

    /// The reserved identifiers are not requests, so a driver that
    /// treated them as one would start a hint sequence the device never
    /// asked for.
    #[test]
    fn the_reserved_command_identifiers_are_not_requests() {
        let device = device();
        device
            .transport
            .set_config_u32(CONFIG_FREE_PAGE_HINT_CMD_ID, 0);
        assert_eq!(device.free_page_hint_cmd_id(), None);
        device
            .transport
            .set_config_u32(CONFIG_FREE_PAGE_HINT_CMD_ID, 1);
        assert_eq!(device.free_page_hint_cmd_id(), None);
    }

    #[test]
    fn statistics_are_encoded_as_tag_value_pairs() {
        let device = device();
        let stats = [
            MemoryStat {
                tag: MemoryStatTag::Total,
                value: 0x1122_3344,
            },
            MemoryStat {
                tag: MemoryStatTag::Free,
                value: 7,
            },
        ];
        let mut submit = pin!(device.submit_stats(&stats));
        assert!(block_on(poll_once(submit.as_mut())).is_none());

        let payload = device
            .stats
            .as_ref()
            .expect("stats queue")
            .queue
            .try_lock()
            .expect("idle queue")
            .device_request(0);
        assert_eq!(payload.len(), 20);
        assert_eq!(u16::from_le_bytes([payload[0], payload[1]]), 5);
        assert_eq!(
            u64::from_le_bytes(payload[2..10].try_into().expect("eight bytes")),
            0x1122_3344
        );
        assert_eq!(u16::from_le_bytes([payload[10], payload[11]]), 4);
        assert_eq!(
            u64::from_le_bytes(payload[12..20].try_into().expect("eight bytes")),
            7
        );

        complete(&device, 2, 0);
        assert_eq!(block_on(poll_once(submit.as_mut())), Some(Ok(())));
    }

    /// A balloon is driven by several tasks at once, each parked on a
    /// different queue. A device-wide notification would let one of them
    /// consume the wake-up that belonged to another, leaving that
    /// other's completion in the used ring with nobody to reap it — and
    /// the host waiting forever for a guest that never answers again.
    #[test]
    fn a_completion_wakes_the_queue_it_belongs_to_even_while_another_waits() {
        let dma = arena();
        let device = device_with(ALL_BALLOON_FEATURES, dma.clone());
        let mut pages = Pages::new(&dma, 1);
        let mut inflated = [pages.range(1)];
        let mut free = Pages::new(&dma, 1);
        let mut reported = [free.range(1)];

        let mut inflate = pin!(device.inflate(&mut inflated));
        let mut report = pin!(device.report_free(&mut reported));
        assert!(block_on(poll_once(inflate.as_mut())).is_none());
        assert!(block_on(poll_once(report.as_mut())).is_none());

        // The device finishes only the report.
        complete(&device, 4, 0);
        assert_eq!(block_on(poll_once(report.as_mut())), Some(Ok(())));
        assert!(
            block_on(poll_once(inflate.as_mut())).is_none(),
            "the inflate request is still with the device"
        );

        complete(&device, 0, 0);
        assert_eq!(block_on(poll_once(inflate.as_mut())), Some(Ok(())));
    }

    /// A configuration-change interrupt is the only way the driver
    /// learns the host moved the target, so it has to wake a waiter
    /// that a used-buffer interrupt would not.
    #[test]
    fn a_configuration_change_wakes_its_own_waiters() {
        let device = device();
        let mut changed = pin!(device.config_changed());
        assert!(block_on(poll_once(changed.as_mut())).is_none());

        device.transport.raise_interrupt(USED_BUFFER_INTERRUPT);
        device.handle_interrupt();
        assert!(
            block_on(poll_once(changed.as_mut())).is_none(),
            "a used-buffer interrupt is not a configuration change"
        );

        device.transport.raise_interrupt(CONFIG_CHANGE_INTERRUPT);
        device.handle_interrupt();
        assert_eq!(block_on(poll_once(changed.as_mut())), Some(()));
    }
}
