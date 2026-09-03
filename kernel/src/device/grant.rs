//! The grant itself: what one out-of-kernel driver may touch, and the
//! machinery that takes it all back.

use core::fmt;

use helios_hal::{
    device::{DeviceRegion, DmaCapable, RegionAttribute},
    iommu::DomainId,
    pmm::{PhysFrame, PhysFrameRange},
    vmm::{PageFlags, VirtRange},
};
use thiserror::Error;

/// Register windows one grant may carry. Six is the number of base
/// address registers a PCI function has; a transport with fewer uses
/// fewer, and a device that wants more than one function's worth of
/// windows is more than one device.
pub const MAX_DEVICE_REGIONS: usize = 6;

/// Interrupt sources one grant may carry. Covers a legacy line plus a
/// small MSI-X vector set; a device with more vectors than this shares
/// them, which is what the interrupt stream in the WIT interface
/// reports anyway.
pub const MAX_DEVICE_INTERRUPTS: usize = 8;

/// Live DMA regions one grant may hold at once. A ring-based NIC needs
/// a descriptor ring plus its buffer pool per direction; sixteen leaves
/// room for a multi-queue device without letting a driver turn the
/// frame allocator into its private heap.
pub const MAX_DEVICE_DMA_REGIONS: usize = 16;

/// A window the kernel opened inside the owning instance's linear
/// memory, named both ways: the guest sees `offset`, the page tables
/// see `virt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestWindow {
    /// Byte offset from the base of the instance's linear memory. This
    /// is what the driver indexes with an ordinary load or store.
    pub offset: u64,
    /// The same window as the address space names it.
    pub virt: VirtRange,
}

/// A device register window placed in the owning instance's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedRegion {
    /// Which of the grant's regions this is.
    pub index: usize,
    /// Where the driver finds it.
    pub offset: u64,
    /// How much of the window is the device, starting at `offset`. The
    /// mapping itself is page-rounded; this is the part the firmware
    /// actually described.
    pub byte_len: usize,
    window: GuestWindow,
}

/// A run of pinned frames a driver may point the device's DMA engine
/// at, mapped into the driver's own memory so it can fill descriptors
/// without a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaRegion {
    /// Where the driver finds it in its own memory.
    pub offset: u64,
    /// The address to write into a descriptor. Under an IOMMU this is
    /// the address the device issues and the domain translates; without
    /// one it is the bus address directly.
    pub device_address: u64,
    /// Length in bytes, page-rounded up from the request.
    pub byte_len: usize,
    window: GuestWindow,
    frames: PhysFrameRange,
}

/// What a [`DeviceGrant::reclaim`] actually did, so the caller can log
/// it and a test can assert on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GrantReclaim {
    /// Register windows taken out of the dead instance's memory.
    pub regions_unmapped: usize,
    /// Interrupt sources masked.
    pub sources_masked: usize,
    /// DMA runs returned to the frame allocator.
    pub dma_returned: usize,
    /// DMA runs held back because the platform cannot prove the device
    /// stopped reaching them. See the module documentation.
    pub dma_quarantined: usize,
    /// Whether the endpoint left its translation domain.
    pub detached: bool,
}

/// The platform operations a grant performs on behalf of a driver that
/// does not live in the kernel.
///
/// One trait rather than four handles because these operations are not
/// independent: reclaim has to perform all of them, in order, on a
/// device whose driver is already gone, and an implementation that can
/// do some of them is not usable.
pub trait DeviceHost {
    /// How this platform names one interrupt this device raises.
    type Source: Copy + Eq + fmt::Debug;

    /// Whatever the platform's mapping, pinning and translation layers
    /// fail with.
    type Error: core::error::Error;

    /// Stop the device mastering the bus, and keep it stopped.
    ///
    /// This is the step that makes the rest of reclaim safe, and the
    /// only one the kernel can perform without knowing what the device
    /// is: on PCI it clears bus-master enable in configuration space.
    /// A platform whose transport has no such control says so by
    /// returning an error, and its frames are quarantined instead of
    /// returned.
    fn quiesce(&self) -> Result<(), Self::Error>;

    /// Reserve `byte_len` bytes of the owning instance's linear memory,
    /// page-aligned, for the kernel to map something into. The instance
    /// must not already have the range committed.
    fn reserve_window(&self, byte_len: usize) -> Result<GuestWindow, Self::Error>;

    /// Give a reserved window back to the instance's allocator.
    fn release_window(&self, window: GuestWindow);

    /// Place `physical` at `window` in the owning instance's memory.
    fn map_physical(
        &self,
        window: GuestWindow,
        physical: PhysFrameRange,
        flags: PageFlags,
    ) -> Result<(), Self::Error>;

    /// Take a mapping back out.
    fn unmap_physical(&self, window: GuestWindow) -> Result<(), Self::Error>;

    /// Stop delivering one of this device's interrupts.
    fn mask(&self, source: Self::Source);

    /// Start delivering it again.
    fn unmask(&self, source: Self::Source);

    /// Take a contiguous run of frames out of the allocator and pin it.
    fn pin_frames(&self, frame_count: usize) -> Result<PhysFrameRange, Self::Error>;

    /// Return a pinned run. Only ever called once the device is known
    /// to have stopped reaching it.
    fn unpin_frames(&self, frames: PhysFrameRange);

    /// The address the device issues to reach `frames`. Under an IOMMU
    /// this installs the translation and returns the I/O virtual
    /// address; without one it is the frames' own bus address.
    fn device_address(
        &self,
        domain: Option<DomainId>,
        frames: PhysFrameRange,
    ) -> Result<u64, Self::Error>;

    /// Withdraw a translation installed by [`Self::device_address`].
    fn withdraw_device_address(&self, domain: Option<DomainId>, address: u64, byte_len: usize);

    /// Take the endpoint out of its translation domain, so DMA still in
    /// flight faults rather than landing.
    fn detach_domain(&self, domain: DomainId) -> Result<(), Self::Error>;
}

/// Everything that can go wrong handing a device to a driver.
#[derive(Debug, Error)]
pub enum DeviceGrantError<PlatformError: core::error::Error> {
    /// More register windows than a grant carries.
    #[error("a device grant carries at most {MAX_DEVICE_REGIONS} regions, was given {count}")]
    TooManyRegions { count: usize },

    /// More interrupt sources than a grant carries.
    #[error("a device grant carries at most {MAX_DEVICE_INTERRUPTS} interrupts, was given {count}")]
    TooManyInterrupts { count: usize },

    /// The driver asked for a region this device does not have.
    #[error("this device has no region {index}")]
    UnknownRegion { index: usize },

    /// The driver asked for a region it already mapped. Mapping twice
    /// would put the same registers at two offsets, which is a driver
    /// bug worth reporting rather than quietly satisfying.
    #[error("region {index} is already mapped at offset {offset:#x}")]
    RegionAlreadyMapped { index: usize, offset: u64 },

    /// The driver acknowledged or unmasked an interrupt that is not
    /// this device's.
    #[error("this device does not raise the named interrupt")]
    UnknownInterrupt,

    /// The driver holds as many DMA regions as a grant allows.
    #[error("a device grant holds at most {MAX_DEVICE_DMA_REGIONS} DMA regions at once")]
    TooManyDmaRegions,

    /// The request would take the driver over its pinned-memory budget.
    #[error("the DMA budget has {remaining} bytes left, {requested} were requested")]
    DmaBudgetExhausted { requested: usize, remaining: usize },

    /// The allocator returned frames this device's DMA engine cannot
    /// address. A 32-bit engine handed a frame above 4 GiB would write
    /// to the truncated address instead, which is memory corruption
    /// with no error report anywhere.
    #[error("frames at {physical:#x} are out of reach of a {address_width_bits}-bit DMA engine")]
    UnaddressableDma {
        physical: usize,
        address_width_bits: u8,
    },

    /// The driver freed a DMA region it does not hold.
    #[error("this device holds no DMA region at offset {offset:#x}")]
    UnknownDmaRegion { offset: u64 },

    /// The platform's mapping, pinning or translation layer failed.
    #[error(transparent)]
    Platform(#[from] PlatformError),
}

/// One device, handed to exactly one driver.
///
/// The grant is owned by whoever supervises that driver — it is not
/// shared, not cloned, and not reachable from the driver's own memory.
/// Every operation the driver performs arrives as a call on the owner's
/// side of the WIT interface, which is why these take `&mut self` and
/// there is no lock anywhere in this type.
pub struct DeviceGrant<Host: DeviceHost> {
    host: Host,
    regions: [Option<DeviceRegion>; MAX_DEVICE_REGIONS],
    region_count: usize,
    sources: [Option<Host::Source>; MAX_DEVICE_INTERRUPTS],
    source_count: usize,
    dma: DmaCapable,
    dma_budget_bytes: usize,

    mapped: [Option<MappedRegion>; MAX_DEVICE_REGIONS],
    dma_regions: [Option<DmaRegion>; MAX_DEVICE_DMA_REGIONS],
    dma_bytes_in_use: usize,
    masked: [bool; MAX_DEVICE_INTERRUPTS],
    quarantine: [Option<PhysFrameRange>; MAX_DEVICE_DMA_REGIONS],
    reclaimed: bool,
}

impl<Host: DeviceHost> DeviceGrant<Host> {
    /// Build a grant over what a backend discovered.
    pub fn new(
        host: Host,
        regions: &[DeviceRegion],
        sources: &[Host::Source],
        dma: DmaCapable,
        dma_budget_bytes: usize,
    ) -> Result<Self, DeviceGrantError<Host::Error>> {
        if regions.len() > MAX_DEVICE_REGIONS {
            return Err(DeviceGrantError::TooManyRegions {
                count: regions.len(),
            });
        }
        if sources.len() > MAX_DEVICE_INTERRUPTS {
            return Err(DeviceGrantError::TooManyInterrupts {
                count: sources.len(),
            });
        }

        let mut stored_regions = [None; MAX_DEVICE_REGIONS];
        for (slot, region) in stored_regions.iter_mut().zip(regions) {
            *slot = Some(*region);
        }
        let mut stored_sources = [None; MAX_DEVICE_INTERRUPTS];
        for (slot, source) in stored_sources.iter_mut().zip(sources) {
            *slot = Some(*source);
        }

        Ok(Self {
            host,
            regions: stored_regions,
            region_count: regions.len(),
            sources: stored_sources,
            source_count: sources.len(),
            dma,
            dma_budget_bytes,
            mapped: [None; MAX_DEVICE_REGIONS],
            dma_regions: [None; MAX_DEVICE_DMA_REGIONS],
            dma_bytes_in_use: 0,
            masked: [false; MAX_DEVICE_INTERRUPTS],
            quarantine: [None; MAX_DEVICE_DMA_REGIONS],
            reclaimed: false,
        })
    }

    /// How many register windows this device has.
    pub const fn region_count(&self) -> usize {
        self.region_count
    }

    /// The interrupts this device raises.
    pub fn sources(&self) -> impl Iterator<Item = Host::Source> + '_ {
        self.sources[..self.source_count]
            .iter()
            .filter_map(|source| *source)
    }

    /// Pinned bytes this driver is currently holding.
    pub const fn dma_bytes_in_use(&self) -> usize {
        self.dma_bytes_in_use
    }

    /// Frames held back by reclaim because the device could not be
    /// proven stopped.
    pub fn quarantined(&self) -> impl Iterator<Item = PhysFrameRange> + '_ {
        self.quarantine.iter().filter_map(|frames| *frames)
    }

    /// Place one of this device's register windows in the driver's
    /// linear memory.
    ///
    /// The mapping is page-rounded, so the returned `offset` points at
    /// the register block itself rather than at the page it starts in.
    pub fn map_region(
        &mut self,
        index: usize,
    ) -> Result<MappedRegion, DeviceGrantError<Host::Error>> {
        self.assert_live();
        let region = self
            .regions
            .get(index)
            .copied()
            .flatten()
            .ok_or(DeviceGrantError::UnknownRegion { index })?;

        if let Some(existing) = self.mapped[index] {
            return Err(DeviceGrantError::RegionAlreadyMapped {
                index,
                offset: existing.offset,
            });
        }

        let frames = region.frames();
        let window = self
            .host
            .reserve_window(frames.frame_count * PhysFrame::SIZE)?;

        let flags = match region.attribute {
            RegionAttribute::Registers => PageFlags::READ | PageFlags::WRITE | PageFlags::DEVICE,
            RegionAttribute::Memory => PageFlags::READ | PageFlags::WRITE,
        };
        if let Err(error) = self.host.map_physical(window, frames, flags) {
            self.host.release_window(window);
            return Err(error.into());
        }

        let mapped = MappedRegion {
            index,
            offset: window.offset + region.page_offset() as u64,
            byte_len: region.byte_len,
            window,
        };
        self.mapped[index] = Some(mapped);
        Ok(mapped)
    }

    /// Pin `byte_len` bytes for the device to read or write, and map
    /// them into the driver's memory so it can fill them without a copy.
    pub fn dma_alloc(
        &mut self,
        byte_len: usize,
    ) -> Result<DmaRegion, DeviceGrantError<Host::Error>> {
        self.assert_live();
        let byte_len = byte_len.next_multiple_of(PhysFrame::SIZE);
        let remaining = self.dma_budget_bytes - self.dma_bytes_in_use;
        if byte_len > remaining {
            return Err(DeviceGrantError::DmaBudgetExhausted {
                requested: byte_len,
                remaining,
            });
        }
        let slot = self
            .dma_regions
            .iter()
            .position(Option::is_none)
            .ok_or(DeviceGrantError::TooManyDmaRegions)?;

        let frames = self.host.pin_frames(byte_len / PhysFrame::SIZE)?;
        let highest = frames.start.phys_addr() + byte_len - 1;
        if !self.dma.can_address(highest) {
            self.host.unpin_frames(frames);
            return Err(DeviceGrantError::UnaddressableDma {
                physical: highest,
                address_width_bits: self.dma.address_width_bits,
            });
        }

        let device_address = match self.host.device_address(self.dma.domain, frames) {
            Ok(address) => address,
            Err(error) => {
                self.host.unpin_frames(frames);
                return Err(error.into());
            }
        };
        let window = match self.host.reserve_window(byte_len) {
            Ok(window) => window,
            Err(error) => {
                self.host
                    .withdraw_device_address(self.dma.domain, device_address, byte_len);
                self.host.unpin_frames(frames);
                return Err(error.into());
            }
        };
        if let Err(error) =
            self.host
                .map_physical(window, frames, PageFlags::READ | PageFlags::WRITE)
        {
            self.host.release_window(window);
            self.host
                .withdraw_device_address(self.dma.domain, device_address, byte_len);
            self.host.unpin_frames(frames);
            return Err(error.into());
        }

        let dma = DmaRegion {
            offset: window.offset,
            device_address,
            byte_len,
            window,
            frames,
        };
        self.dma_regions[slot] = Some(dma);
        self.dma_bytes_in_use += byte_len;
        Ok(dma)
    }

    /// Release one DMA region the driver is done with.
    ///
    /// This is the driver saying the device has stopped using it, which
    /// the kernel takes at face value for a region the driver still
    /// owns — the device can only reach frames this same driver
    /// programmed it with. Reclaim after a crash is the case where that
    /// assurance is gone, and it is handled differently.
    pub fn dma_free(&mut self, offset: u64) -> Result<(), DeviceGrantError<Host::Error>> {
        self.assert_live();
        let slot = self
            .dma_regions
            .iter()
            .position(|region| region.is_some_and(|region| region.offset == offset))
            .ok_or(DeviceGrantError::UnknownDmaRegion { offset })?;
        let region = self.dma_regions[slot]
            .take()
            .expect("the slot was occupied");

        self.host.unmap_physical(region.window)?;
        self.host.release_window(region.window);
        self.host
            .withdraw_device_address(self.dma.domain, region.device_address, region.byte_len);
        self.host.unpin_frames(region.frames);
        self.dma_bytes_in_use -= region.byte_len;
        Ok(())
    }

    /// Acknowledge an interrupt and let it fire again.
    pub fn unmask(&mut self, source: Host::Source) -> Result<(), DeviceGrantError<Host::Error>> {
        self.assert_live();
        let index = self.source_index(source)?;
        self.host.unmask(source);
        self.masked[index] = false;
        Ok(())
    }

    /// Stop one of this device's interrupts from firing.
    pub fn mask(&mut self, source: Host::Source) -> Result<(), DeviceGrantError<Host::Error>> {
        self.assert_live();
        let index = self.source_index(source)?;
        self.host.mask(source);
        self.masked[index] = true;
        Ok(())
    }

    /// Take the whole device back from a driver that is gone.
    ///
    /// Safe to call on a grant in any state, including one whose driver
    /// died between two of these very steps, which is the case it
    /// exists for. See the module documentation for why the order is
    /// what it is, and when frames are quarantined rather than returned.
    pub fn reclaim(&mut self) -> GrantReclaim {
        let mut report = GrantReclaim::default();

        let quiesced = match self.host.quiesce() {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(
                    %error,
                    "could not stop this device mastering the bus; its DMA frames stay quarantined"
                );
                false
            }
        };

        for source in self.sources[..self.source_count].iter().filter_map(|s| *s) {
            self.host.mask(source);
            report.sources_masked += 1;
        }
        self.masked = [true; MAX_DEVICE_INTERRUPTS];

        if let Some(domain) = self.dma.domain {
            match self.host.detach_domain(domain) {
                Ok(()) => report.detached = true,
                Err(error) => tracing::error!(
                    %error,
                    %domain,
                    "could not detach this device's endpoint from its domain"
                ),
            }
        }

        for slot in &mut self.mapped {
            let Some(region) = slot.take() else { continue };
            if let Err(error) = self.host.unmap_physical(region.window) {
                tracing::error!(
                    %error,
                    index = region.index,
                    "could not unmap a device register window from a dead instance"
                );
            }
            self.host.release_window(region.window);
            report.regions_unmapped += 1;
        }

        // A detached endpoint can no longer reach the frames whatever
        // the device thinks it is doing; a quiesced one on a platform
        // with no IOMMU is merely believed to have stopped. Only the
        // first is proof.
        let safe_to_return = report.detached || quiesced;
        for slot in &mut self.dma_regions {
            let Some(region) = slot.take() else { continue };
            if let Err(error) = self.host.unmap_physical(region.window) {
                tracing::error!(
                    %error,
                    offset = region.offset,
                    "could not unmap a DMA region from a dead instance"
                );
            }
            self.host.release_window(region.window);
            self.host.withdraw_device_address(
                self.dma.domain,
                region.device_address,
                region.byte_len,
            );
            self.dma_bytes_in_use -= region.byte_len;

            if safe_to_return {
                self.host.unpin_frames(region.frames);
                report.dma_returned += 1;
            } else {
                let quarantine = self
                    .quarantine
                    .iter_mut()
                    .find(|slot| slot.is_none())
                    .expect("the quarantine has one slot per live DMA region");
                *quarantine = Some(region.frames);
                report.dma_quarantined += 1;
            }
        }

        self.reclaimed = true;
        report
    }

    /// Return quarantined frames to the allocator, once the owner has
    /// proven the device stopped — a bus reset, a power cycle, or the
    /// platform gaining an IOMMU. Called with nothing quarantined this
    /// does nothing.
    pub fn release_quarantine(&mut self) -> usize {
        let mut released = 0;
        for slot in &mut self.quarantine {
            if let Some(frames) = slot.take() {
                self.host.unpin_frames(frames);
                released += 1;
            }
        }
        released
    }

    fn source_index(&self, source: Host::Source) -> Result<usize, DeviceGrantError<Host::Error>> {
        self.sources[..self.source_count]
            .iter()
            .position(|candidate| *candidate == Some(source))
            .ok_or(DeviceGrantError::UnknownInterrupt)
    }

    fn assert_live(&self) {
        assert!(
            !self.reclaimed,
            "a reclaimed device grant was used again; its driver is gone and its \
             registers are unmapped, so every operation on it is a kernel bug"
        );
    }
}

#[cfg(test)]
mod tests;
