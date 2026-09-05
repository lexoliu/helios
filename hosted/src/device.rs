//! A device for a machine that has none.
//!
//! The hosted backend has no bus to walk and no controller to program,
//! but the kernel's device-grant path is hardware-independent: what it
//! needs is a physical range it can map, a source it can hold off, and
//! memory it can pin. This module supplies all three out of the host
//! process, so map, unmap, interrupt delivery, pinning and reclaim can
//! be exercised end to end without hardware.
//!
//! The device's registers are an ordinary host allocation, backed by an
//! unlinked temporary file and mapped shared. That backing is what makes
//! them a *device*: file-backed pages can appear at a second address, so
//! the kernel mapping them into an owner's memory produces a real alias
//! — a write through the owner's mapping is visible through the
//! backend's, exactly as a register write is visible to hardware. An
//! anonymous mapping could not do that, and copying would test nothing.
//!
//! # Concurrency contract
//!
//! The published devices are built once, during bring-up, and read from
//! every thread afterwards. `mask` and `unmask` record what a controller
//! would have been told; there is no controller, so nothing races.

use std::ffi::CString;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use helios_hal::device::{DeviceRegion, DeviceRegionAttributes, DmaCapability};
use helios_hal::iommu::{DmaTranslation, PhysicalRange};
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpace, AddressSpaceError, PageFlags, VirtRange};
use helios_kernel::{
    DeviceGrant, DeviceInterruptHooks, DeviceName, DeviceVmHooks, DmaBudget, GrantError,
    GrantInterrupt, install_device_interrupt_hooks, install_device_vm_hooks,
};

use crate::vmm::{HostedAddressSpace, host_mapping_granule};

/// The name the hosted machine publishes its device under.
pub const HOSTED_DEVICE_NAME: &str = "hosted:device0";

/// The interrupt the hosted device raises.
///
/// Nothing delivers it on its own: a test forwards it through the
/// registry, which is what a controller would have done.
pub const HOSTED_DEVICE_INTERRUPT: u32 = 1;

/// Bytes of registers the hosted device exposes.
///
/// Two host pages, so a test can prove that a second region lands at its
/// own granule rather than inside the first one's page.
fn register_bytes() -> usize {
    2 * host_mapping_granule() as usize
}

/// Most memory the kernel will pin for the hosted device.
const HOSTED_DMA_BUDGET_BYTES: u64 = 1 << 20;

/// One host-backed range standing in for a device's registers.
struct HostedDeviceMemory {
    /// The unlinked temporary file the pages live in. Held open for the
    /// life of the process: the mapping the kernel installs for an owner
    /// is made from it.
    descriptor: i32,
    /// Where the backend's own view of the registers lives, which is
    /// what a hosted "physical address" is.
    base: usize,
    bytes: usize,
}

/// Where the host pages behind a region live.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeviceBacking {
    pub(crate) descriptor: i32,
    pub(crate) offset: libc::off_t,
}

impl HostedDeviceMemory {
    /// Allocate `bytes` of shared, file-backed memory and map the
    /// backend's own view of it.
    ///
    /// # Panics
    ///
    /// Panics when the host refuses the allocation. A hosted machine
    /// that cannot make a temporary file has no device to publish and no
    /// weaker device worth publishing instead.
    fn new(bytes: usize) -> Self {
        let template = CString::new("/tmp/helios-hosted-device-XXXXXX")
            .expect("the template holds no interior nul");
        let mut path = template.into_bytes_with_nul();
        // SAFETY: `path` is a nul-terminated, writable buffer holding a
        // template `mkstemp` accepts.
        let descriptor = unsafe { libc::mkstemp(path.as_mut_ptr().cast()) };
        assert!(
            descriptor >= 0,
            "the hosted device needs a temporary file for its registers"
        );
        // SAFETY: `path` is the nul-terminated name `mkstemp` filled in.
        let unlinked = unsafe { libc::unlink(path.as_ptr().cast()) };
        assert!(
            unlinked == 0,
            "the hosted device's backing file must not outlive the process"
        );
        // SAFETY: `descriptor` is a fresh, empty regular file.
        let sized = unsafe { libc::ftruncate(descriptor, bytes as libc::off_t) };
        assert!(sized == 0, "the hosted device's registers must be sizeable");
        // SAFETY: the descriptor names a regular file of exactly
        // `bytes`, and the kernel picks the address.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                descriptor,
                0,
            )
        };
        assert!(
            base != libc::MAP_FAILED,
            "the hosted device's registers must be mappable"
        );
        Self {
            descriptor,
            base: base as usize,
            bytes,
        }
    }

    fn region(&self, attributes: DeviceRegionAttributes) -> DeviceRegion {
        DeviceRegion::new(
            PhysicalRange::new(self.base as u64, self.bytes as u64),
            attributes,
        )
    }

    fn backing_for(&self, region: &DeviceRegion) -> Option<DeviceBacking> {
        let start = region.physical.start as usize;
        let end = start.checked_add(region.physical.bytes as usize)?;
        if start < self.base || end > self.base + self.bytes {
            return None;
        }
        Some(DeviceBacking {
            descriptor: self.descriptor,
            offset: (start - self.base) as libc::off_t,
        })
    }

    /// The backend's own view of the registers, for a test that wants to
    /// prove an owner's write reached the device.
    fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is live for the life of the process and
        // covers exactly `bytes`.
        unsafe { std::slice::from_raw_parts(self.base as *const u8, self.bytes) }
    }
}

/// Everything the hosted machine's device path owns: the address space
/// the kernel maps through, the device's memory, and what a controller
/// would have been told.
struct HostedDevicePlatform {
    address_space: HostedAddressSpace,
    memory: HostedDeviceMemory,
    masked: AtomicU64,
    unmasked: AtomicU64,
}

static PLATFORM: OnceLock<HostedDevicePlatform> = OnceLock::new();

fn platform() -> &'static HostedDevicePlatform {
    PLATFORM.get_or_init(|| HostedDevicePlatform {
        address_space: HostedAddressSpace::new(),
        memory: HostedDeviceMemory::new(register_bytes()),
        masked: AtomicU64::new(0),
        unmasked: AtomicU64::new(0),
    })
}

pub(crate) fn backing_for(region: &DeviceRegion) -> Option<DeviceBacking> {
    PLATFORM
        .get()
        .and_then(|platform| platform.memory.backing_for(region))
}

/// The address space the hosted device's mappings live in.
///
/// A caller reserves the owner's linear memory out of this space, so
/// the kernel's mappings land where the owner would see them.
pub fn device_address_space() -> &'static HostedAddressSpace {
    &platform().address_space
}

/// The backend's own view of the device's registers.
pub fn device_registers() -> &'static [u8] {
    platform().memory.as_slice()
}

/// How many times a controller would have been told to hold the
/// device's source off, and to let it through again.
pub fn interrupt_controller_counts() -> (u64, u64) {
    let platform = platform();
    (
        platform.masked.load(Ordering::Relaxed),
        platform.unmasked.load(Ordering::Relaxed),
    )
}

fn map_device(virt: VirtRange, region: DeviceRegion) -> Result<(), AddressSpaceError> {
    platform().address_space.map_device(virt, region)
}

fn unmap_device(virt: VirtRange) -> Result<(), AddressSpaceError> {
    platform().address_space.unmap_device(virt)
}

fn commit_contiguous(
    virt: VirtRange,
    flags: PageFlags,
    limit: u64,
) -> Result<PhysFrame, AddressSpaceError> {
    platform()
        .address_space
        .commit_contiguous(virt, flags, limit)
}

fn decommit(virt: VirtRange) -> Result<(), AddressSpaceError> {
    platform().address_space.decommit(virt)
}

fn mapping_granule() -> u64 {
    host_mapping_granule()
}

fn mask(source: u32) {
    debug_assert_eq!(source, HOSTED_DEVICE_INTERRUPT);
    platform().masked.fetch_add(1, Ordering::Relaxed);
}

fn unmask(source: u32) {
    debug_assert_eq!(source, HOSTED_DEVICE_INTERRUPT);
    platform().unmasked.fetch_add(1, Ordering::Relaxed);
}

static VM_HOOKS: DeviceVmHooks = DeviceVmHooks {
    map_device,
    unmap_device,
    commit_contiguous,
    decommit,
    mapping_granule,
};

static INTERRUPT_HOOKS: DeviceInterruptHooks = DeviceInterruptHooks { mask, unmask };

/// Install the hosted machine's device surface and build the one grant
/// it publishes.
///
/// Called during bring-up, before the registry publishes anything. The
/// grant is a single register file with one interrupt and a megabyte of
/// pinnable memory: the smallest shape that exercises every part of the
/// path.
pub fn hosted_device_grants() -> Result<[DeviceGrant; 1], GrantError> {
    install_device_vm_hooks(&VM_HOOKS);
    install_device_interrupt_hooks(&INTERRUPT_HOOKS);
    let platform = platform();
    let grant = DeviceGrant::new(
        DeviceName::new(HOSTED_DEVICE_NAME)?,
        DmaBudget {
            capability: DmaCapability {
                // The host process's own addresses are what this device
                // would issue, so its reach is the machine's.
                address_bits: usize::BITS,
                coherent: true,
                translation: DmaTranslation::direct(),
            },
            byte_budget: HOSTED_DMA_BUDGET_BYTES,
        },
    )
    .with_region(platform.memory.region(DeviceRegionAttributes::REGISTERS))?
    .with_interrupt(GrantInterrupt::new(HOSTED_DEVICE_INTERRUPT))?;
    Ok([grant])
}
