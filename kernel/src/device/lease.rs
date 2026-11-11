//! One owner's hold on one device.
//!
//! A lease is what the registry hands the supervisor that provisions a
//! driver: it carries the grant, the relay the device's interrupts
//! arrive on, and what the one owner that currently has it can reach
//! the device through. Everything the owner can reach the device through
//! is recorded here, so the kernel can prove — by walking one structure
//! — that the device is unreachable again once the owner lets go.
//!
//! # Where a device appears in its owner's memory
//!
//! Helios owns the user address space, so a granted region does not need
//! a call per register: the kernel shows the device's own frames at a
//! span of the owner's linear memory the owner chose, and a register
//! access becomes an ordinary load or store. A ring the driver pins is a
//! [`Buffer`] shown the same way. Both are views in the owner's
//! [`ViewTable`], in one [`ViewGroup`] the lease names, and both end
//! together when the grant does: a span the owner chose is inside its
//! memory's current size, so nothing the owner grows can land on it.
//!
//! # Concurrency contract
//!
//! A lease is owned by one task — the task running the owner's store —
//! and is never shared, so its state needs no lock. The relay it points
//! at *is* shared with interrupt context and carries its own
//! synchronisation. The views are the store's and change only on that
//! same task; the buffers behind the rings are shared with nothing but
//! those views, and the last of the two to let go frees the run on
//! whichever task that is.

use arrayvec::ArrayVec;
use helios_hal::device::DmaPlacement;
use triomphe::Arc;

use crate::buffer::{Buffer, GuestMemory, Span, ViewError, ViewGroup, ViewId, ViewTable};

use super::grant::{DeviceGrant, GrantError, MAX_GRANT_REGIONS};
use super::interrupt::{InterruptRelay, InterruptStats};

/// Buffers one owner may pin for its device at once.
pub const MAX_DMA_BUFFERS: usize = 16;

/// One pinned, physically contiguous buffer the device reads and writes
/// directly, as its owner is told about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaBuffer {
    /// How long the buffer is: the span it is shown at.
    pub bytes: u64,
    /// The address the device has to issue to reach it, which is a
    /// physical address on a machine with no translation unit in the
    /// path and an I/O virtual address on one that confines the device.
    pub device_address: u64,
}

/// One region shown in the owner's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShownRegion {
    view: ViewId,
    bytes: u64,
}

/// One pinned buffer as the lease records it: the device's attachment
/// to the run, held until the grant ends.
struct PinnedDma {
    /// Held, never read: this reference is what keeps the run pinned
    /// while the device may still address it.
    _attachment: Buffer,
    descriptor: DmaBuffer,
}

/// What one owner's hold on one device has cost, for the stats panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GrantStats {
    /// Regions currently shown in the owner's memory.
    pub mapped_regions: u32,
    /// Bytes of those regions.
    pub mapped_bytes: u64,
    /// Buffers currently pinned for the device.
    pub dma_buffers: u32,
    /// Bytes of them, against the grant's budget.
    pub pinned_bytes: u64,
    /// What the device's interrupts have done.
    pub interrupts: InterruptStats,
}

/// What the registry keeps for one published device: the grant itself
/// and the relay its interrupts arrive on.
///
/// Shared, because a backend registers its interrupt route against the
/// relay at boot and that route outlives every owner the device has.
pub struct PublishedDevice {
    pub(super) grant: DeviceGrant,
    pub(super) relay: InterruptRelay,
    pub(super) claimed: core::sync::atomic::AtomicBool,
}

impl PublishedDevice {
    pub fn grant(&self) -> &DeviceGrant {
        &self.grant
    }

    pub fn relay(&self) -> &InterruptRelay {
        &self.relay
    }

    /// Whether the device currently has an owner.
    pub fn is_claimed(&self) -> bool {
        self.claimed.load(core::sync::atomic::Ordering::Acquire)
    }
}

/// One owner's exclusive hold on one device.
///
/// Letting go is [`Self::end_views`], which ends every view the owner
/// had of the device, followed by dropping the lease. Dropping a lease
/// without ending its views first is the path a dying store takes: its view table has already
/// withdrawn every view by then — the table is dropped before the
/// store's device state — so what is left is masking every source,
/// releasing every ring and returning the device to the registry, and
/// an owner that is killed rather than shut down cleanly still leaves
/// the device free for its replacement.
pub struct GrantLease {
    device: Arc<PublishedDevice>,
    /// The set every view of this device in the owner's memory belongs
    /// to.
    views: ViewGroup,
    /// Where each of the grant's regions is shown, by region index.
    regions: [Option<ShownRegion>; MAX_GRANT_REGIONS],
    buffers: ArrayVec<PinnedDma, MAX_DMA_BUFFERS>,
    pinned_bytes: u64,
}

impl GrantLease {
    pub(super) fn new(device: Arc<PublishedDevice>, views: ViewGroup) -> Self {
        Self {
            device,
            views,
            regions: [const { None }; MAX_GRANT_REGIONS],
            buffers: ArrayVec::new(),
            pinned_bytes: 0,
        }
    }

    pub fn grant(&self) -> &DeviceGrant {
        &self.device.grant
    }

    pub fn relay(&self) -> &InterruptRelay {
        &self.device.relay
    }

    /// The set the owner's views of this device belong to.
    pub const fn views(&self) -> ViewGroup {
        self.views
    }

    /// Show the grant's `index`-th region at `at` of the owner's memory,
    /// for as long as the grant lives.
    ///
    /// `at` is the region's size rounded up to whole wasm pages. A
    /// region is shown once: asking again is refused rather than
    /// answered, because the second span would be a second path to the
    /// same registers that the owner's allocator believes is free.
    pub fn map_region(
        &mut self,
        index: usize,
        views: &mut ViewTable,
        memory: GuestMemory,
        at: Span,
    ) -> Result<(), GrantError> {
        let region = *self
            .device
            .grant
            .regions()
            .get(index)
            .ok_or(GrantError::NoSuchRegion)?;
        if self.regions[index].is_some() {
            return Err(GrantError::RegionAlreadyMapped);
        }
        let view = views.show_device(memory, at, region, Some(self.views))?;
        self.regions[index] = Some(ShownRegion {
            view,
            bytes: region.physical.bytes,
        });
        Ok(())
    }

    /// Pin a physically contiguous buffer the device can reach, as long
    /// as `at` and aligned to `align`, and show it there, zeroed.
    ///
    /// The buffer counts against the grant's budget and is charged to
    /// the owner's own pool, so a driver cannot pin more than the kernel
    /// budgeted for its device.
    pub fn dma_alloc(
        &mut self,
        align: u64,
        views: &mut ViewTable,
        memory: GuestMemory,
        at: Span,
    ) -> Result<DmaBuffer, GrantError> {
        if at.length == 0 {
            return Err(GrantError::EmptyRequest);
        }
        if align == 0 || !align.is_power_of_two() {
            return Err(GrantError::BadAlignment);
        }
        if self.buffers.is_full() {
            return Err(GrantError::BudgetExhausted);
        }
        let budget = self.device.grant.dma();
        // The sum is checked: with something already pinned, a span near
        // the top of the range overflows it.
        if self
            .pinned_bytes
            .checked_add(at.length)
            .is_none_or(|pinned| pinned > budget.byte_budget)
        {
            return Err(GrantError::BudgetExhausted);
        }
        // The span is checked before anything is pinned, so a driver
        // that names the wrong pages costs the machine nothing.
        views.validate(memory, at, at.length)?;
        let buffer = Buffer::allocate(
            at.length,
            DmaPlacement {
                align,
                limit: budget.capability.address_limit(),
            },
        )?;
        let physical = buffer.physical();
        if !budget.capability.can_reach(physical.start, physical.bytes) {
            // The address space handed back a run the device cannot
            // address. Nothing is salvageable from a buffer at the wrong
            // end of memory; dropping it gives it back.
            return Err(GrantError::Unreachable);
        }
        let device_address = budget
            .capability
            .translation
            .device_range(physical.start, physical.bytes)
            .map_err(|_| GrantError::Unreachable)?;
        views.show_buffer(memory, at, &buffer, Some(self.views))?;
        let descriptor = DmaBuffer {
            bytes: physical.bytes,
            device_address,
        };
        self.pinned_bytes += physical.bytes;
        self.buffers.push(PinnedDma {
            _attachment: buffer,
            descriptor,
        });
        Ok(descriptor)
    }

    /// The buffers currently pinned for the device.
    pub fn dma_buffers(&self) -> impl Iterator<Item = DmaBuffer> + '_ {
        self.buffers.iter().map(|pinned| pinned.descriptor)
    }

    pub fn stats(&self) -> GrantStats {
        GrantStats {
            mapped_regions: self.regions.iter().flatten().count() as u32,
            mapped_bytes: self
                .regions
                .iter()
                .flatten()
                .map(|region| region.bytes)
                .sum(),
            dma_buffers: self.buffers.len() as u32,
            pinned_bytes: self.pinned_bytes,
            interrupts: self.device.relay.stats(),
        }
    }

    /// End every view the owner has of the device, which is the first
    /// half of taking it back: dropping the lease afterwards masks
    /// every source, releases every pinned buffer and returns the
    /// device to the registry.
    ///
    /// Called by a supervisor that is restarting its driver and by a
    /// driver letting go. Fails only when a view cannot be ended for
    /// want of memory to put back, in which case the lease is still
    /// whole: the owner is out of memory, and its death takes the same
    /// path with nothing left to allocate.
    pub fn end_views(&self, views: &mut ViewTable) -> Result<(), ViewError> {
        views.end_group(self.views)
    }
}

impl Drop for GrantLease {
    fn drop(&mut self) {
        self.device.relay.quiesce();
        let buffers = self.buffers.len();
        // The device's attachment to every ring goes here; a ring whose
        // view is somehow still live keeps its run until that view ends,
        // so nothing the owner can still reach is freed.
        self.buffers.clear();
        self.device
            .claimed
            .store(false, core::sync::atomic::Ordering::Release);
        tracing::info!(
            target: "helios_kernel::device",
            device = %self.device.grant.name(),
            regions = self.regions.iter().flatten().count(),
            buffers,
            "device grant reclaimed"
        );
    }
}
