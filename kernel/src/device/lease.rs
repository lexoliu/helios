//! One owner's hold on one device.
//!
//! A lease is what the registry hands the supervisor that provisions a
//! driver: it carries the grant, the relay the device's interrupts
//! arrive on, and the mapping state of the one owner that currently has
//! it. Everything the owner can reach the device through is recorded
//! here, so the kernel can prove — by walking one structure — that the
//! device is unreachable again once the owner dies.
//!
//! # Where a device appears in its owner's memory
//!
//! Helios owns the user address space, so a granted region does not need
//! a call per register: the kernel maps the device's own frames inside
//! the owner's linear-memory reservation and a register access becomes
//! an ordinary load or store. The mappings go at the top of that
//! reservation, in the [`DeviceWindow`], above everything the owner's
//! memory can ever grow to; the owner's growth limit is capped below the
//! window so a `memory.grow` can never land on a register file.
//!
//! # Concurrency contract
//!
//! A lease is owned by one task — the supervisor that provisioned the
//! driver, and through it the driver's own store — and is never shared,
//! so its mapping state needs no lock. The relay it points at *is*
//! shared with interrupt context and carries its own synchronisation.
//! Every mapping change goes through the address space, which
//! invalidates the local translation cache and shoots down every other
//! processor that has run in the space before it returns.

use arrayvec::ArrayVec;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{PageFlags, VirtAddr, VirtRange};
use triomphe::Arc;

use super::grant::{DeviceGrant, GrantError, MAX_GRANT_REGIONS};
use super::interrupt::{InterruptRelay, InterruptStats};
use super::platform::device_vm_hooks;

/// Bytes at the top of a granted owner's linear-memory reservation the
/// kernel keeps for that owner's device.
///
/// Sized to hold every region a device exposes plus the rings a driver
/// pins for it: a 16 MiB aperture and a few megabytes of descriptors is
/// the large end of what non-virtio hardware asks for, and the window
/// costs an owner nothing it can use, because the memory it displaces
/// sits above the four gigabytes a 32-bit instance can address anyway on
/// every reservation the kernel builds.
pub const DEVICE_WINDOW_BYTES: u64 = 64 << 20;

/// Buffers one owner may pin for its device at once.
pub const MAX_DMA_BUFFERS: usize = 16;

/// The part of one owner's linear memory the kernel devotes to its
/// device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceWindow {
    /// Where the owner's linear memory starts in the address space.
    base: VirtAddr,
    /// Byte offset of the window within that linear memory.
    offset: u64,
    /// How long the window is.
    bytes: u64,
}

impl DeviceWindow {
    /// The window at the top of a `reservation_bytes`-long linear-memory
    /// reservation based at `base`.
    ///
    /// # Panics
    ///
    /// Panics when the reservation is too small to carry a window. Every
    /// reservation the kernel builds is four gigabytes; a smaller one is
    /// a configuration the device path was never designed against, and
    /// placing the window inside addressable memory would let the owner
    /// grow over its own device.
    pub fn top_of(base: VirtAddr, reservation_bytes: u64) -> Self {
        assert!(
            reservation_bytes > DEVICE_WINDOW_BYTES,
            "a linear-memory reservation of {reservation_bytes} bytes cannot carry a \
             {DEVICE_WINDOW_BYTES}-byte device window"
        );
        Self {
            base,
            offset: reservation_bytes - DEVICE_WINDOW_BYTES,
            bytes: DEVICE_WINDOW_BYTES,
        }
    }

    /// The offset the window starts at, which is also the highest the
    /// owner's memory may grow to.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The address a `bytes`-long span at `offset` within the window
    /// occupies.
    fn range_at(&self, offset: u64, bytes: u64) -> VirtRange {
        let start = self.base.raw() as u64 + offset;
        VirtRange::new(
            VirtAddr::new(usize::try_from(start).expect("a window sits inside the address space")),
            usize::try_from(bytes).expect("a window span fits the address space"),
        )
    }
}

/// Where one of a device's regions ended up in its owner's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedRegion {
    /// Byte offset in the owner's linear memory.
    pub offset: u64,
    /// How many bytes of it the region covers.
    pub bytes: u64,
}

/// One pinned, physically contiguous buffer the device reads and writes
/// directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaBuffer {
    /// Byte offset in the owner's linear memory.
    pub offset: u64,
    /// How long the buffer is.
    pub bytes: u64,
    /// The address the device has to issue to reach it, which is a
    /// physical address on a machine with no translation unit in the
    /// path and an I/O virtual address on one that confines the device.
    pub device_address: u64,
}

/// What one owner's hold on one device has cost, for the stats panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GrantStats {
    /// Regions currently mapped into the owner's memory.
    pub mapped_regions: u32,
    /// Bytes of those mappings.
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
/// Dropping a lease reclaims everything the owner could reach the device
/// through and returns the device to the registry, so an owner that is
/// killed rather than shut down cleanly still leaves the device free for
/// its replacement.
pub struct GrantLease {
    device: Arc<PublishedDevice>,
    window: DeviceWindow,
    /// Where each of the grant's regions landed, by region index.
    regions: [Option<MappedRegion>; MAX_GRANT_REGIONS],
    /// Bump cursor into the window. Regions and buffers are handed out
    /// in the order they are asked for and released together, because a
    /// driver builds its rings once and holds them for as long as it
    /// holds the device.
    cursor: u64,
    buffers: ArrayVec<DmaBuffer, MAX_DMA_BUFFERS>,
    pinned_bytes: u64,
}

impl GrantLease {
    pub(super) fn new(device: Arc<PublishedDevice>, window: DeviceWindow) -> Self {
        Self {
            device,
            window,
            regions: [const { None }; MAX_GRANT_REGIONS],
            cursor: 0,
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

    pub const fn window(&self) -> DeviceWindow {
        self.window
    }

    /// Map the grant's `index`-th region into the owner's memory, or
    /// report where it already is.
    ///
    /// Mapping is what a driver does once per region during its own
    /// bring-up, so asking twice is answered rather than refused: a
    /// driver that lost track of an offset is not a driver that has
    /// corrupted anything.
    pub fn map_region(&mut self, index: usize) -> Result<MappedRegion, GrantError> {
        let region = *self
            .device
            .grant
            .regions()
            .get(index)
            .ok_or(GrantError::NoSuchRegion)?;
        if let Some(mapped) = self.regions[index] {
            return Ok(mapped);
        }
        let bytes = (region.frame_count() as u64) * (PhysFrame::SIZE as u64);
        let offset = self.carve(bytes, PhysFrame::SIZE as u64)?;
        let virt = self.window.range_at(offset, bytes);
        (device_vm_hooks().map_device)(virt, region)?;
        let mapped = MappedRegion {
            offset: self.window.offset + offset,
            bytes,
        };
        self.regions[index] = Some(mapped);
        Ok(mapped)
    }

    /// Pin a physically contiguous buffer the device can reach, inside
    /// the owner's memory.
    ///
    /// The buffer counts against the grant's budget and against the
    /// owner's own memory accounting: the pages come from the user pool,
    /// so a driver that pins more than the machine can spare is killed
    /// like any other instance that asked for too much.
    pub fn dma_alloc(&mut self, bytes: u64, align: u64) -> Result<DmaBuffer, GrantError> {
        if bytes == 0 {
            return Err(GrantError::BudgetExhausted);
        }
        if align == 0 || !align.is_power_of_two() {
            return Err(GrantError::BadAlignment);
        }
        if self.buffers.is_full() {
            return Err(GrantError::BudgetExhausted);
        }
        let budget = self.device.grant.dma();
        let frame = PhysFrame::SIZE as u64;
        let bytes = bytes.next_multiple_of(frame);
        if self.pinned_bytes + bytes > budget.byte_budget {
            return Err(GrantError::BudgetExhausted);
        }
        let offset = self.carve(bytes, align.max(frame))?;
        let virt = self.window.range_at(offset, bytes);
        let first = (device_vm_hooks().commit_contiguous)(
            virt,
            PageFlags::READ | PageFlags::WRITE,
            budget.capability.address_limit(),
        )?;
        let physical = first.phys_addr() as u64;
        if !budget.capability.can_reach(physical, bytes) {
            // The address space handed back a run the device cannot
            // address. Nothing is salvageable from a buffer at the wrong
            // end of memory, and leaving it committed would leak it.
            (device_vm_hooks().decommit)(virt)?;
            return Err(GrantError::Unreachable);
        }
        let device_address = budget
            .capability
            .translation
            .device_range(physical, bytes)
            .map_err(|_| GrantError::Unreachable)?;
        let buffer = DmaBuffer {
            offset: self.window.offset + offset,
            bytes,
            device_address,
        };
        self.buffers.push(buffer);
        self.pinned_bytes += bytes;
        Ok(buffer)
    }

    /// The buffers currently pinned for the device.
    pub fn dma_buffers(&self) -> &[DmaBuffer] {
        &self.buffers
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

    /// Take the device back: mask every source, unmap every region,
    /// release every pinned buffer, and return the device to the
    /// registry.
    ///
    /// Called explicitly by a supervisor that is restarting its driver
    /// and by [`Drop`] otherwise, so the two paths cannot diverge.
    ///
    /// # Panics
    ///
    /// Panics when the address space refuses to undo a mapping. The
    /// kernel cannot then prove the dead owner has lost its last path to
    /// the device's registers, and continuing would hand that path to
    /// whoever the next owner is.
    pub fn reclaim(self) {
        drop(self);
    }

    /// Carve `bytes` at `align` out of the window.
    fn carve(&mut self, bytes: u64, align: u64) -> Result<u64, GrantError> {
        let start = self
            .cursor
            .checked_next_multiple_of(align)
            .ok_or(GrantError::WindowExhausted)?;
        let end = start
            .checked_add(bytes)
            .ok_or(GrantError::WindowExhausted)?;
        if end > self.window.bytes {
            return Err(GrantError::WindowExhausted);
        }
        self.cursor = end;
        Ok(start)
    }
}

impl Drop for GrantLease {
    fn drop(&mut self) {
        self.device.relay.quiesce();
        for mapped in self.regions.iter().flatten() {
            let offset = mapped.offset - self.window.offset;
            let virt = self.window.range_at(offset, mapped.bytes);
            (device_vm_hooks().unmap_device)(virt).unwrap_or_else(|error| {
                panic!(
                    "device {} kept a mapping the address space would not undo: {error}",
                    self.device.grant.name()
                )
            });
        }
        for buffer in &self.buffers {
            let offset = buffer.offset - self.window.offset;
            let virt = self.window.range_at(offset, buffer.bytes);
            (device_vm_hooks().decommit)(virt).unwrap_or_else(|error| {
                panic!(
                    "device {} kept a pinned buffer the address space would not release: {error}",
                    self.device.grant.name()
                )
            });
        }
        self.device
            .claimed
            .store(false, core::sync::atomic::Ordering::Release);
        tracing::info!(
            target: "helios_kernel::device",
            device = %self.device.grant.name(),
            regions = self.regions.iter().flatten().count(),
            buffers = self.buffers.len(),
            "device grant reclaimed"
        );
    }
}
