//! Memory-balloon device contract.
//!
//! A balloon is how a host takes memory back from a guest that is not
//! using it, and how the guest tells the host which of its memory is
//! idle. Both directions are device operations — a target the host
//! publishes, page-frame numbers the guest hands over, runs of free
//! memory it names — so the contract belongs here, beside the other
//! hardware contracts, rather than in whichever backend happens to
//! carry the device.
//!
//! The memory a balloon moves is named in the kernel's direct map, as
//! everywhere else in [`crate::pmm`]; a driver translates to bus
//! addresses the same way it does for every other buffer.
//!
//! # SMP contract
//!
//! Every method takes `&self` and may be called from any processor. The
//! asynchronous ones park on the device's completion notification, so a
//! caller yields rather than spinning.

use core::future::Future;

use alloc::sync::Arc;

use crate::io::IoError;

/// A statistic the guest publishes about its own memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryStatTag {
    /// Memory the guest has free, in bytes.
    Free,
    /// Memory the guest manages, in bytes.
    Total,
    /// Memory the guest could hand to new work without reclaiming, in
    /// bytes.
    Available,
}

/// One published memory statistic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryStat {
    pub tag: MemoryStatTag,
    pub value: u64,
}

/// The device side of a memory balloon.
///
/// A driver implements it; the kernel owns the policy and never names a
/// transport.
pub trait MemoryBalloon: Send + Sync + 'static {
    /// How many 4 KiB pages the host wants the balloon to hold.
    fn target_pages(&self) -> u32;

    /// Publishes how many pages the balloon actually holds.
    fn set_actual(&self, pages: u32);

    /// Whether an inflated page may only be reused after the host has
    /// been told through the deflate path.
    fn must_tell_host(&self) -> bool;

    /// Whether the driver may deflate under memory pressure without the
    /// host lowering its target first.
    fn deflates_on_oom(&self) -> bool;

    /// Whether the device accepts unsolicited free-page reports.
    fn reports_free_pages(&self) -> bool;

    /// Whether the device carries a statistics queue.
    fn publishes_stats(&self) -> bool;

    /// The hint command the device is asking for, if any.
    fn free_page_hint_cmd_id(&self) -> Option<u32>;

    /// Resolves when the device changes its configuration space.
    fn config_changed(&self) -> impl Future<Output = ()> + Send + '_;

    /// Hands `ranges` to the host.
    fn inflate<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a;

    /// Takes `ranges` back from the host.
    fn deflate<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a;

    /// Tells the host that `ranges` are free without giving them up.
    fn report_free<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a;

    /// Opens a free-page hint sequence.
    fn begin_free_page_hint(
        &self,
        cmd_id: u32,
    ) -> impl Future<Output = Result<(), IoError>> + Send + '_;

    /// Names free memory inside an open hint sequence.
    fn hint_free_pages<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a;

    /// Closes an open hint sequence.
    fn end_free_page_hint(&self) -> impl Future<Output = Result<(), IoError>> + Send + '_;

    /// Publishes the guest's view of its own memory. Resolves when the
    /// host consumes it, which is the host asking for the next one.
    fn submit_stats<'a>(
        &'a self,
        stats: &'a [MemoryStat],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a;
}

/// A shared balloon is a balloon.
///
/// The kernel gives the device to several tasks at once — one follows
/// the host's target, one reports free memory, one answers the
/// statistics queue — so what it holds has to be cheap to clone. A
/// driver is a single owned object; this is what lets a backend hand it
/// over without writing a forwarding wrapper of its own.
impl<Device: MemoryBalloon> MemoryBalloon for Arc<Device> {
    fn target_pages(&self) -> u32 {
        Device::target_pages(self)
    }

    fn set_actual(&self, pages: u32) {
        Device::set_actual(self, pages);
    }

    fn must_tell_host(&self) -> bool {
        Device::must_tell_host(self)
    }

    fn deflates_on_oom(&self) -> bool {
        Device::deflates_on_oom(self)
    }

    fn reports_free_pages(&self) -> bool {
        Device::reports_free_pages(self)
    }

    fn publishes_stats(&self) -> bool {
        Device::publishes_stats(self)
    }

    fn free_page_hint_cmd_id(&self) -> Option<u32> {
        Device::free_page_hint_cmd_id(self)
    }

    fn config_changed(&self) -> impl Future<Output = ()> + Send + '_ {
        Device::config_changed(self)
    }

    fn inflate<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
        Device::inflate(self, ranges)
    }

    fn deflate<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
        Device::deflate(self, ranges)
    }

    fn report_free<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
        Device::report_free(self, ranges)
    }

    fn begin_free_page_hint(
        &self,
        cmd_id: u32,
    ) -> impl Future<Output = Result<(), IoError>> + Send + '_ {
        Device::begin_free_page_hint(self, cmd_id)
    }

    fn hint_free_pages<'a>(
        &'a self,
        ranges: &'a mut [&'a mut [u8]],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
        Device::hint_free_pages(self, ranges)
    }

    fn end_free_page_hint(&self) -> impl Future<Output = Result<(), IoError>> + Send + '_ {
        Device::end_free_page_hint(self)
    }

    fn submit_stats<'a>(
        &'a self,
        stats: &'a [MemoryStat],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
        Device::submit_stats(self, stats)
    }
}
