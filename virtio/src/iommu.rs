//! virtio-iommu driver (virtio 1.3 §5.13, device id 23).
//!
//! The device translates the DMA of the endpoints attached to its
//! domains. The driver owns two virtqueues: the request queue carries
//! `ATTACH`/`DETACH`/`MAP`/`UNMAP`/`PROBE` commands, and the event queue
//! carries translation faults the device reports back.
//!
//! Concurrency contract: requests are issued from bring-up and teardown
//! paths, never from a data path — helios maps a domain once and then
//! translates in software through [`helios_hal::iommu::DmaTranslation`],
//! so no submission ever waits on this device. A request therefore
//! completes by polling the used ring, the same register-level handshake
//! the transports already spin on for device reset, rather than by
//! parking a task. The queue locks are held only across a submission or
//! a poll, so the device may answer on another processor while the
//! caller waits. Fault events arrive on the device's own interrupt and
//! are drained by [`VirtioIommuDevice::handle_interrupt`].

use alloc::boxed::Box;
use alloc::vec;
use core::sync::atomic::{AtomicU64, Ordering};

use helios_hal::io::{IoError, IoResult};
use helios_hal::iommu::{
    DmaAccess, DomainId, EndpointId, IoVirtAddr, Iommu, IommuError, IommuGeometry,
};
use spin::Mutex;

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate_with};
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

/// Request queue index (virtio 1.3 §5.13.2).
const REQUEST_QUEUE_INDEX: u16 = 0;
/// Event queue index; the device reports translation faults on it.
const EVENT_QUEUE_INDEX: u16 = 1;
const REQUEST_QUEUE_SIZE: u16 = 16;
const EVENT_QUEUE_SIZE: u16 = 16;
/// A request is one readable body plus one writable reply.
const REQUEST_CHAIN_LIMIT: u16 = 2;
/// A fault event is a single writable buffer.
const EVENT_CHAIN_LIMIT: u16 = 1;

/// `VIRTIO_IOMMU_F_INPUT_RANGE`: the device restricts the addresses a
/// domain may map.
const F_INPUT_RANGE: u64 = 1 << 0;
/// `VIRTIO_IOMMU_F_DOMAIN_RANGE`: the device restricts the domain ids.
const F_DOMAIN_RANGE: u64 = 1 << 1;
/// `VIRTIO_IOMMU_F_MAP_UNMAP`: the device implements `MAP` and `UNMAP`.
const F_MAP_UNMAP: u64 = 1 << 2;
/// `VIRTIO_IOMMU_F_PROBE`: the device implements `PROBE`.
const F_PROBE: u64 = 1 << 4;
/// `VIRTIO_IOMMU_F_BYPASS_CONFIG`: the device exposes a global bypass
/// switch in its configuration space.
const F_BYPASS_CONFIG: u64 = 1 << 6;

/// Everything this driver knows how to use. `MAP_UNMAP` is mandatory:
/// without it a domain cannot be given any memory at all.
const IOMMU_FEATURES: u64 =
    F_INPUT_RANGE | F_DOMAIN_RANGE | F_MAP_UNMAP | F_PROBE | F_BYPASS_CONFIG;

/// `struct virtio_iommu_config` field offsets.
const CONFIG_PAGE_SIZE_MASK: usize = 0x00;
const CONFIG_INPUT_RANGE_START: usize = 0x08;
const CONFIG_INPUT_RANGE_END: usize = 0x10;
const CONFIG_DOMAIN_RANGE_START: usize = 0x18;
const CONFIG_DOMAIN_RANGE_END: usize = 0x1c;
const CONFIG_PROBE_SIZE: usize = 0x20;
const CONFIG_BYPASS: usize = 0x24;

/// Request types (`struct virtio_iommu_req_head::type`).
const T_ATTACH: u8 = 0x01;
const T_DETACH: u8 = 0x02;
const T_MAP: u8 = 0x03;
const T_UNMAP: u8 = 0x04;
const T_PROBE: u8 = 0x05;

/// Status codes (`struct virtio_iommu_req_tail::status`).
const S_OK: u8 = 0x00;
const S_IOERR: u8 = 0x01;
const S_UNSUPP: u8 = 0x02;
const S_DEVERR: u8 = 0x03;
const S_INVAL: u8 = 0x04;
const S_RANGE: u8 = 0x05;
const S_NOENT: u8 = 0x06;
const S_FAULT: u8 = 0x07;
const S_NOMEM: u8 = 0x08;

/// `VIRTIO_IOMMU_MAP_F_*` mapping flags.
const MAP_F_READ: u32 = 1 << 0;
const MAP_F_WRITE: u32 = 1 << 1;
const MAP_F_MMIO: u32 = 1 << 2;

/// Probe property types (`struct virtio_iommu_probe_property::type`).
const PROBE_T_MASK: u16 = 0xfff;
const PROBE_T_NONE: u16 = 0;
const PROBE_T_RESV_MEM: u16 = 1;

/// `VIRTIO_IOMMU_RESV_MEM_T_MSI`: the range is an interrupt doorbell the
/// endpoint has to keep reaching.
const RESV_MEM_T_MSI: u8 = 1;

/// Every request body is shorter than this; `PROBE` is the longest at
/// 72 bytes.
const MAX_REQUEST_BYTES: usize = 72;
/// Bytes of `struct virtio_iommu_req_tail`.
const TAIL_BYTES: usize = 4;
/// The largest probe buffer this driver will accept from a device.
const MAX_PROBE_BYTES: usize = 512;
/// Bytes of one `struct virtio_iommu_fault`.
const FAULT_BYTES: usize = 32;
/// `struct virtio_iommu_fault` field offsets.
const FAULT_REASON: usize = 0x00;
const FAULT_FLAGS: usize = 0x04;
const FAULT_ENDPOINT: usize = 0x08;
const FAULT_ADDRESS: usize = 0x10;

/// How many reserved regions one endpoint may report.
pub const MAX_RESERVED_REGIONS: usize = 8;

/// One physical range an endpoint has to keep reaching even though its
/// domain confines the rest of its DMA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReservedRegion {
    /// First physical byte of the range.
    pub start: u64,
    /// Length of the range in bytes.
    pub bytes: u64,
    /// Whether the range is an interrupt doorbell, which the endpoint
    /// must be able to write, as opposed to a range that is simply not
    /// translatable.
    pub doorbell: bool,
}

/// The virtio-iommu device.
pub struct VirtioIommuDevice<T: VirtioTransport> {
    transport: T,
    /// Serialises whole requests and owns the buffers they use.
    ///
    /// The device answers one request at a time, so a single set of
    /// buffers is all the driver needs, and holding this across the poll
    /// is what keeps a second caller from reusing them.
    request: Mutex<RequestBuffers>,
    request_queue: Mutex<VirtQueue<T>>,
    event_queue: Mutex<EventQueue<T>>,
    geometry: IommuGeometry,
    probe_bytes: usize,
    features: NegotiatedFeatures,
    faults: AtomicU64,
    requests: AtomicU64,
}

/// The request body and reply buffers, kept on the heap so their
/// addresses are ones the DMA pool can translate.
struct RequestBuffers {
    body: Box<[u8]>,
    reply: Box<[u8]>,
}

/// The event queue plus the buffers the device writes faults into.
struct EventQueue<T: VirtioTransport> {
    queue: VirtQueue<T>,
    buffers: Box<[u8]>,
    /// Which fault buffer each in-flight descriptor identifier carries.
    ///
    /// A re-armed buffer is not guaranteed the identifier it had before
    /// — the ring hands identifiers back in completion order — so the
    /// pairing is recorded at submission rather than assumed.
    slot_for_token: [u16; EVENT_QUEUE_SIZE as usize],
}

impl<T: VirtioTransport> EventQueue<T> {
    /// Hands one fault buffer to the device.
    fn arm(&mut self, transport: &T, slot: u16) -> IoResult<()> {
        let buffer = fault_slot(&mut self.buffers, slot);
        let token = self.queue.submit(transport, &[], &mut [buffer])?;
        self.slot_for_token[usize::from(token)] = slot;
        Ok(())
    }
}

/// The bytes of one fault record inside the event buffer slab.
fn fault_slot(buffers: &mut [u8], slot: u16) -> &mut [u8] {
    let start = usize::from(slot) * FAULT_BYTES;
    &mut buffers[start..start + FAULT_BYTES]
}

impl<T: VirtioTransport> VirtioIommuDevice<T> {
    /// Brings up a virtio-iommu function.
    ///
    /// The device is left with `DRIVER_OK` set and its event queue
    /// stocked, but with no domain built: which endpoints exist and what
    /// each of their domains maps is decided a layer up.
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Iommu {
            return Err(IoError::Unsupported);
        }

        let features = negotiate_with(&transport, |offered| {
            RING_FEATURES | (IOMMU_FEATURES & offered)
        })?;
        if !features.device(F_MAP_UNMAP) {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-iommu device does not implement MAP/UNMAP",
            ));
        }

        let geometry = read_geometry(&transport, features);
        let probe_bytes = if features.device(F_PROBE) {
            let declared = usize::try_from(transport.read_config_u32(CONFIG_PROBE_SIZE))
                .map_err(|_| IoError::OutOfBounds)?;
            if declared > MAX_PROBE_BYTES {
                return Err(IoError::InvalidDeviceConfig(
                    "virtio-iommu probe buffer is larger than this driver accepts",
                ));
            }
            declared
        } else {
            0
        };

        let request_queue = build_queue(
            &transport,
            REQUEST_QUEUE_INDEX,
            REQUEST_QUEUE_SIZE,
            REQUEST_CHAIN_LIMIT,
            features,
        )?;
        let event_ring = build_queue(
            &transport,
            EVENT_QUEUE_INDEX,
            EVENT_QUEUE_SIZE,
            EVENT_CHAIN_LIMIT,
            features,
        )?;
        let event_slots = event_ring.available_descriptors();
        let mut events = EventQueue {
            queue: event_ring,
            buffers: vec![0_u8; event_slots * FAULT_BYTES].into_boxed_slice(),
            slot_for_token: [0; EVENT_QUEUE_SIZE as usize],
        };
        stock_event_queue(&mut events, &transport)?;

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        Ok(Self {
            transport,
            request: Mutex::new(RequestBuffers {
                body: vec![0_u8; MAX_REQUEST_BYTES].into_boxed_slice(),
                reply: vec![0_u8; probe_bytes + TAIL_BYTES].into_boxed_slice(),
            }),
            request_queue: Mutex::new(request_queue),
            event_queue: Mutex::new(events),
            geometry,
            probe_bytes,
            features,
            faults: AtomicU64::new(0),
            requests: AtomicU64::new(0),
        })
    }

    /// The feature set this device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    /// Whether the device passes untranslated DMA through for endpoints
    /// that are attached to no domain.
    ///
    /// An endpoint that *is* attached is always translated, so this says
    /// nothing about a confined device — it says whether the rest of the
    /// machine's devices still reach memory.
    pub fn global_bypass(&self) -> bool {
        self.features.device(F_BYPASS_CONFIG) && self.transport.read_config_u8(CONFIG_BYPASS) != 0
    }

    /// Requests the device has answered since boot.
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Translation faults the device has reported since boot.
    pub fn fault_count(&self) -> u64 {
        self.faults.load(Ordering::Relaxed)
    }

    /// Acknowledges the device interrupt and drains the fault events it
    /// carried.
    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        self.drain_faults();
    }

    /// Reads back every fault the device has posted and re-arms the
    /// buffers it used.
    pub fn drain_faults(&self) {
        let mut state = self.event_queue.lock();
        let EventQueue {
            queue,
            buffers,
            slot_for_token,
        } = &mut *state;
        let mut returned = [0_u16; EVENT_QUEUE_SIZE as usize];
        let mut drained = 0;
        queue.drain_used(|token, _len| {
            let slot = slot_for_token[usize::from(token)];
            let start = usize::from(slot) * FAULT_BYTES;
            let fault = decode_fault(&buffers[start..start + FAULT_BYTES]);
            tracing::error!(
                reason = fault.reason,
                flags = fault.flags,
                endpoint = fault.endpoint,
                address = fault.address,
                "virtio-iommu reported a translation fault"
            );
            returned[drained] = slot;
            drained += 1;
        });
        if drained == 0 {
            return;
        }
        self.faults.fetch_add(drained as u64, Ordering::Relaxed);
        for slot in returned[..drained].iter().copied() {
            state.arm(&self.transport, slot).unwrap_or_else(|error| {
                panic!("virtio-iommu event buffer could not be re-armed: {error}")
            });
        }
        state.queue.notify(&self.transport);
    }

    /// Asks the device which physical ranges `endpoint` has to keep
    /// reaching, writing them into `regions`.
    ///
    /// Returns the number of regions written. A device without
    /// `VIRTIO_IOMMU_F_PROBE` reports none, which is not an error: the
    /// platform then supplies its own doorbell range.
    pub fn probe(
        &self,
        endpoint: EndpointId,
        regions: &mut [ReservedRegion; MAX_RESERVED_REGIONS],
    ) -> Result<usize, IommuError> {
        if !self.features.device(F_PROBE) {
            return Ok(0);
        }
        let mut buffers = self.request.lock();
        let body = encode_probe(&mut buffers.body, endpoint);
        let reply_bytes = self.probe_bytes + TAIL_BYTES;
        self.exchange(&mut buffers, body, reply_bytes)?;
        Ok(decode_reserved_regions(
            &buffers.reply[..self.probe_bytes],
            regions,
        ))
    }

    /// Runs one request to completion and reports what the device made
    /// of it.
    fn exchange(
        &self,
        buffers: &mut RequestBuffers,
        body_bytes: usize,
        reply_bytes: usize,
    ) -> Result<(), IommuError> {
        let RequestBuffers { body, reply } = buffers;
        reply[..reply_bytes].fill(0);
        let token = {
            let mut queue = self.request_queue.lock();
            let token = queue
                .submit(
                    &self.transport,
                    &[&body[..body_bytes]],
                    &mut [&mut reply[..reply_bytes]],
                )
                .map_err(|_| IommuError::DeviceFault)?;
            queue.notify(&self.transport);
            token
        };
        loop {
            let completed = {
                let mut queue = self.request_queue.lock();
                queue.pop_used()
            };
            match completed {
                Some(completed) => {
                    assert_eq!(
                        completed, token,
                        "virtio-iommu answered a request that was never issued"
                    );
                    break;
                }
                // The device answers a request from its own model
                // without waiting on anything the kernel has to do
                // first, so this is a device handshake rather than a
                // wait on software state.
                None => core::hint::spin_loop(),
            }
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        status_result(reply[reply_bytes - TAIL_BYTES])
    }

    /// Encodes and runs a request whose reply is just the status tail.
    fn command(&self, encode: impl FnOnce(&mut [u8]) -> usize) -> Result<(), IommuError> {
        let mut buffers = self.request.lock();
        let body = encode(&mut buffers.body);
        self.exchange(&mut buffers, body, TAIL_BYTES)
    }
}

impl<T: VirtioTransport> Iommu for VirtioIommuDevice<T> {
    fn geometry(&self) -> IommuGeometry {
        self.geometry.clone()
    }

    fn attach(&self, domain: DomainId, endpoint: EndpointId) -> Result<(), IommuError> {
        self.command(|body| encode_attach(body, domain, endpoint))
    }

    fn detach(&self, domain: DomainId, endpoint: EndpointId) -> Result<(), IommuError> {
        self.command(|body| encode_detach(body, domain, endpoint))
    }

    fn map(
        &self,
        domain: DomainId,
        iova: IoVirtAddr,
        physical: u64,
        bytes: u64,
        access: DmaAccess,
    ) -> Result<(), IommuError> {
        let last = mapping_last_address(iova.get(), bytes)?;
        if !self.geometry.input_range.contains(&iova.get())
            || !self.geometry.input_range.contains(&last)
        {
            return Err(IommuError::OutOfRange);
        }
        let granule = self.geometry.granule();
        if !iova.get().is_multiple_of(granule)
            || !physical.is_multiple_of(granule)
            || !bytes.is_multiple_of(granule)
        {
            return Err(IommuError::Invalid);
        }
        self.command(|body| encode_map(body, domain, iova.get(), last, physical, access))
    }

    fn unmap(&self, domain: DomainId, iova: IoVirtAddr, bytes: u64) -> Result<(), IommuError> {
        let last = mapping_last_address(iova.get(), bytes)?;
        self.command(|body| encode_unmap(body, domain, iova.get(), last))
    }
}

impl<T: VirtioTransport> Drop for VirtioIommuDevice<T> {
    fn drop(&mut self) {
        self.event_queue.get_mut().queue.shutdown(&self.transport);
        self.request_queue.get_mut().shutdown(&self.transport);
    }
}

/// One fault the device reported on its event queue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FaultEvent {
    reason: u32,
    flags: u32,
    endpoint: u32,
    address: u64,
}

fn build_queue<T: VirtioTransport>(
    transport: &T,
    index: u16,
    requested_size: u16,
    chain_limit: u16,
    features: NegotiatedFeatures,
) -> IoResult<VirtQueue<T>> {
    let size = transport.queue_max_size(index).min(requested_size);
    if size == 0 || !size.is_power_of_two() {
        return Err(IoError::InvalidDeviceConfig(
            "virtio-iommu queue size is not a usable power of two",
        ));
    }
    VirtQueue::new(transport, index, size, chain_limit, features)
}

/// Hands every event buffer to the device so a fault always has
/// somewhere to land.
fn stock_event_queue<T: VirtioTransport>(state: &mut EventQueue<T>, transport: &T) -> IoResult<()> {
    for slot in 0..state.buffers.len() / FAULT_BYTES {
        let slot = u16::try_from(slot).map_err(|_| IoError::OutOfBounds)?;
        state.arm(transport, slot)?;
    }
    state.queue.notify(transport);
    Ok(())
}

fn read_geometry<T: VirtioTransport>(transport: &T, features: NegotiatedFeatures) -> IommuGeometry {
    let page_size_mask = read_config_u64(transport, CONFIG_PAGE_SIZE_MASK);
    assert!(
        page_size_mask != 0,
        "virtio-iommu offered no mappable page size"
    );
    let input_range = if features.device(F_INPUT_RANGE) {
        read_config_u64(transport, CONFIG_INPUT_RANGE_START)
            ..=read_config_u64(transport, CONFIG_INPUT_RANGE_END)
    } else {
        0..=u64::MAX
    };
    let domain_range = if features.device(F_DOMAIN_RANGE) {
        transport.read_config_u32(CONFIG_DOMAIN_RANGE_START)
            ..=transport.read_config_u32(CONFIG_DOMAIN_RANGE_END)
    } else {
        0..=u32::MAX
    };
    IommuGeometry {
        page_size_mask,
        input_range,
        domain_range,
    }
}

fn read_config_u64<T: VirtioTransport>(transport: &T, offset: usize) -> u64 {
    u64::from(transport.read_config_u32(offset))
        | (u64::from(transport.read_config_u32(offset + 4)) << 32)
}

/// The inclusive last address of a mapping, as virtio-iommu expresses
/// ranges.
fn mapping_last_address(iova: u64, bytes: u64) -> Result<u64, IommuError> {
    if bytes == 0 {
        return Err(IommuError::Invalid);
    }
    iova.checked_add(bytes - 1).ok_or(IommuError::OutOfRange)
}

fn write_u32(body: &mut [u8], offset: usize, value: u32) {
    body[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(body: &mut [u8], offset: usize, value: u64) {
    body[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn write_head(body: &mut [u8], kind: u8, length: usize) {
    body[..length].fill(0);
    body[0] = kind;
}

fn encode_attach(body: &mut [u8], domain: DomainId, endpoint: EndpointId) -> usize {
    const LENGTH: usize = 20;
    write_head(body, T_ATTACH, LENGTH);
    write_u32(body, 4, domain.get());
    write_u32(body, 8, endpoint.get());
    LENGTH
}

fn encode_detach(body: &mut [u8], domain: DomainId, endpoint: EndpointId) -> usize {
    const LENGTH: usize = 20;
    write_head(body, T_DETACH, LENGTH);
    write_u32(body, 4, domain.get());
    write_u32(body, 8, endpoint.get());
    LENGTH
}

fn encode_map(
    body: &mut [u8],
    domain: DomainId,
    first: u64,
    last: u64,
    physical: u64,
    access: DmaAccess,
) -> usize {
    const LENGTH: usize = 36;
    write_head(body, T_MAP, LENGTH);
    write_u32(body, 4, domain.get());
    write_u64(body, 8, first);
    write_u64(body, 16, last);
    write_u64(body, 24, physical);
    write_u32(body, 32, map_flags(access));
    LENGTH
}

fn encode_unmap(body: &mut [u8], domain: DomainId, first: u64, last: u64) -> usize {
    const LENGTH: usize = 28;
    write_head(body, T_UNMAP, LENGTH);
    write_u32(body, 4, domain.get());
    write_u64(body, 8, first);
    write_u64(body, 16, last);
    LENGTH
}

fn encode_probe(body: &mut [u8], endpoint: EndpointId) -> usize {
    const LENGTH: usize = 72;
    write_head(body, T_PROBE, LENGTH);
    write_u32(body, 4, endpoint.get());
    LENGTH
}

fn map_flags(access: DmaAccess) -> u32 {
    let mut flags = 0;
    if access.contains(DmaAccess::READ) {
        flags |= MAP_F_READ;
    }
    if access.contains(DmaAccess::WRITE) {
        flags |= MAP_F_WRITE;
    }
    if access.contains(DmaAccess::MMIO) {
        flags |= MAP_F_MMIO;
    }
    flags
}

fn status_result(status: u8) -> Result<(), IommuError> {
    match status {
        S_OK => Ok(()),
        S_IOERR => Err(IommuError::DeviceFault),
        S_UNSUPP => Err(IommuError::Unsupported),
        S_DEVERR => Err(IommuError::DeviceFault),
        S_INVAL => Err(IommuError::Invalid),
        S_RANGE => Err(IommuError::OutOfRange),
        S_NOENT => Err(IommuError::NotFound),
        S_FAULT => Err(IommuError::Fault),
        S_NOMEM => Err(IommuError::OutOfResources),
        unknown => panic!("virtio-iommu reported unknown status {unknown:#x}"),
    }
}

fn decode_fault(bytes: &[u8]) -> FaultEvent {
    FaultEvent {
        reason: read_u32(bytes, FAULT_REASON),
        flags: read_u32(bytes, FAULT_FLAGS),
        endpoint: read_u32(bytes, FAULT_ENDPOINT),
        address: read_u64(bytes, FAULT_ADDRESS),
    }
}

/// Walks the property list a `PROBE` reply carries and keeps the
/// reserved-memory entries.
fn decode_reserved_regions(
    properties: &[u8],
    regions: &mut [ReservedRegion; MAX_RESERVED_REGIONS],
) -> usize {
    const PROPERTY_HEAD_BYTES: usize = 4;
    const RESV_MEM_BYTES: usize = 20;
    let mut found = 0;
    let mut offset = 0;
    while offset + PROPERTY_HEAD_BYTES <= properties.len() {
        let kind = read_u16(properties, offset) & PROBE_T_MASK;
        let length = usize::from(read_u16(properties, offset + 2));
        if kind == PROBE_T_NONE {
            break;
        }
        let body = offset + PROPERTY_HEAD_BYTES;
        assert!(
            body + length <= properties.len(),
            "virtio-iommu probe property at {offset} runs past the reply"
        );
        if kind == PROBE_T_RESV_MEM {
            assert!(
                length >= RESV_MEM_BYTES,
                "virtio-iommu reserved-memory property is shorter than the layout"
            );
            let start = read_u64(properties, body + 4);
            let last = read_u64(properties, body + 12);
            assert!(
                last >= start,
                "virtio-iommu reported a reserved region that ends before it starts"
            );
            let region = ReservedRegion {
                start,
                bytes: last - start + 1,
                doorbell: properties[body] == RESV_MEM_T_MSI,
            };
            assert!(
                found < regions.len(),
                "virtio-iommu reported more than {MAX_RESERVED_REGIONS} reserved regions"
            );
            regions[found] = region;
            found += 1;
        }
        offset = body + length;
    }
    found
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from(read_u32(bytes, offset)) | (u64::from(read_u32(bytes, offset + 4)) << 32)
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::{
        DmaAccess, DomainId, EndpointId, FAULT_BYTES, IoError, IoVirtAddr, Iommu, IommuError,
        MAX_RESERVED_REGIONS, PROBE_T_RESV_MEM, RESV_MEM_T_MSI, ReservedRegion, S_INVAL, S_NOENT,
        S_OK, T_ATTACH, T_DETACH, T_MAP, T_PROBE, T_UNMAP, VirtioIommuDevice,
        decode_reserved_regions,
    };
    use crate::testing::{FakeTransport, FakeTransportConfig};
    use crate::transport::{DeviceType, VirtioFeatures};

    /// `-(4 * KiB)`, a device that maps from 4 KiB pages upwards.
    const PAGE_SIZE_MASK: u64 = 0xffff_ffff_ffff_f000;
    const PROBE_BYTES: u32 = 64;
    const OFFERED: u64 = VirtioFeatures::VERSION_1.bits()
        | super::F_INPUT_RANGE
        | super::F_DOMAIN_RANGE
        | super::F_MAP_UNMAP
        | super::F_PROBE
        | super::F_BYPASS_CONFIG;

    fn transport() -> FakeTransport {
        let transport = FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Iommu,
            offered_features: OFFERED,
            queue_size: 8,
            supports_queue_reset: false,
        });
        transport.set_config_u32(super::CONFIG_PAGE_SIZE_MASK, PAGE_SIZE_MASK as u32);
        transport.set_config_u32(
            super::CONFIG_PAGE_SIZE_MASK + 4,
            (PAGE_SIZE_MASK >> 32) as u32,
        );
        transport.set_config_u32(super::CONFIG_INPUT_RANGE_END, u32::MAX);
        transport.set_config_u32(super::CONFIG_INPUT_RANGE_END + 4, u32::MAX);
        transport.set_config_u32(super::CONFIG_DOMAIN_RANGE_END, u32::MAX);
        transport.set_config_u32(super::CONFIG_PROBE_SIZE, PROBE_BYTES);
        transport.set_config_u8(super::CONFIG_BYPASS, 1);
        transport
    }

    fn device() -> VirtioIommuDevice<FakeTransport> {
        VirtioIommuDevice::new(transport()).expect("the iommu device should initialize")
    }

    /// Plays the device side of exactly one request: waits for the
    /// driver's submission, hands back `status`, and returns the request
    /// body the driver wrote.
    ///
    /// The driver polls its used ring while it waits, releasing the
    /// queue lock between polls, so a device on another processor is
    /// exactly how a real answer arrives.
    fn answer(
        device: &VirtioIommuDevice<FakeTransport>,
        kicks_before: usize,
        status: u8,
        properties: &[u8],
    ) -> std::vec::Vec<u8> {
        while device.transport.kick_count() == kicks_before {
            thread::yield_now();
        }
        let queue = device.request_queue.lock();
        let request = queue.device_request(0);
        let mut reply = std::vec::Vec::from(properties);
        reply.push(status);
        reply.extend_from_slice(&[0, 0, 0]);
        queue.device_respond(0, &reply);
        let written = u32::try_from(reply.len()).expect("the reply length fits");
        queue.device_complete(0, written);
        request
    }

    /// Runs `request` against a device that answers with `status`.
    fn exchange<R: Send>(
        status: u8,
        properties: &[u8],
        request: impl FnOnce(&VirtioIommuDevice<FakeTransport>) -> R + Send,
    ) -> (R, std::vec::Vec<u8>) {
        let device = device();
        let kicks_before = device.transport.kick_count();
        thread::scope(|scope| {
            let device = &device;
            let responder = scope.spawn(move || answer(device, kicks_before, status, properties));
            let result = request(device);
            (
                result,
                responder.join().expect("the device thread finishes"),
            )
        })
    }

    #[test]
    fn a_wrong_device_type_is_rejected() {
        let rejected = VirtioIommuDevice::new(FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Block,
            ..FakeTransportConfig::default()
        }))
        .err();

        assert_eq!(rejected, Some(IoError::Unsupported));
    }

    #[test]
    fn geometry_comes_from_the_device_configuration() {
        let device = device();
        let geometry = device.geometry();

        assert_eq!(geometry.page_size_mask, PAGE_SIZE_MASK);
        assert_eq!(geometry.granule(), 4096);
        assert_eq!(*geometry.input_range.end(), u64::MAX);
        assert_eq!(*geometry.domain_range.end(), u32::MAX);
        assert!(device.global_bypass());
    }

    #[test]
    fn every_event_buffer_is_handed_to_the_device_at_bring_up() {
        let device = device();
        let queue = device.event_queue.lock();

        // Eight slots, each holding one fault record, all in flight.
        assert_eq!(queue.buffers.len(), 8 * FAULT_BYTES);
        assert_eq!(queue.queue.available_descriptors(), 0);
    }

    #[test]
    fn attach_encodes_the_domain_and_endpoint() {
        let (result, request) = exchange(S_OK, &[], |device| {
            device.attach(DomainId::new(3), EndpointId::new(0x18))
        });

        assert_eq!(result, Ok(()));
        assert_eq!(request.len(), 20);
        assert_eq!(request[0], T_ATTACH);
        assert_eq!(&request[4..8], &3_u32.to_le_bytes());
        assert_eq!(&request[8..12], &0x18_u32.to_le_bytes());
        // The flags word stays clear: helios never asks for a bypass
        // domain.
        assert_eq!(&request[12..16], &0_u32.to_le_bytes());
    }

    #[test]
    fn detach_encodes_the_domain_and_endpoint() {
        let (result, request) = exchange(S_OK, &[], |device| {
            device.detach(DomainId::new(7), EndpointId::new(0x20))
        });

        assert_eq!(result, Ok(()));
        assert_eq!(request[0], T_DETACH);
        assert_eq!(&request[4..8], &7_u32.to_le_bytes());
        assert_eq!(&request[8..12], &0x20_u32.to_le_bytes());
    }

    /// virtio-iommu expresses a range by its inclusive last address, not
    /// by a length; getting that wrong maps one page too many.
    #[test]
    fn map_encodes_an_inclusive_range_and_the_access_flags() {
        let (result, request) = exchange(S_OK, &[], |device| {
            device.map(
                DomainId::new(1),
                IoVirtAddr::new(0x1_0000_0000),
                0x4000_0000,
                0x2000,
                DmaAccess::READ | DmaAccess::WRITE,
            )
        });

        assert_eq!(result, Ok(()));
        assert_eq!(request.len(), 36);
        assert_eq!(request[0], T_MAP);
        assert_eq!(&request[4..8], &1_u32.to_le_bytes());
        assert_eq!(&request[8..16], &0x1_0000_0000_u64.to_le_bytes());
        assert_eq!(&request[16..24], &0x1_0000_1fff_u64.to_le_bytes());
        assert_eq!(&request[24..32], &0x4000_0000_u64.to_le_bytes());
        assert_eq!(&request[32..36], &0b11_u32.to_le_bytes());
    }

    #[test]
    fn a_doorbell_mapping_is_flagged_as_device_memory() {
        let (result, request) = exchange(S_OK, &[], |device| {
            device.map(
                DomainId::new(1),
                IoVirtAddr::new(0xfee0_0000),
                0xfee0_0000,
                0x1000,
                DmaAccess::WRITE | DmaAccess::MMIO,
            )
        });

        assert_eq!(result, Ok(()));
        assert_eq!(&request[32..36], &0b110_u32.to_le_bytes());
    }

    #[test]
    fn unmap_encodes_an_inclusive_range() {
        let (result, request) = exchange(S_OK, &[], |device| {
            device.unmap(DomainId::new(2), IoVirtAddr::new(0x8000), 0x2000)
        });

        assert_eq!(result, Ok(()));
        assert_eq!(request.len(), 28);
        assert_eq!(request[0], T_UNMAP);
        assert_eq!(&request[8..16], &0x8000_u64.to_le_bytes());
        assert_eq!(&request[16..24], &0x9fff_u64.to_le_bytes());
    }

    #[test]
    fn a_misaligned_mapping_never_reaches_the_device() {
        let device = device();

        assert_eq!(
            device.map(
                DomainId::new(1),
                IoVirtAddr::new(0x1001),
                0x4000_0000,
                0x1000,
                DmaAccess::READ,
            ),
            Err(IommuError::Invalid)
        );
        assert_eq!(
            device.map(
                DomainId::new(1),
                IoVirtAddr::new(0x1000),
                0x4000_0000,
                0x800,
                DmaAccess::READ,
            ),
            Err(IommuError::Invalid)
        );
        assert_eq!(device.request_count(), 0);
    }

    #[test]
    fn an_empty_mapping_is_refused() {
        let device = device();

        assert_eq!(
            device.map(
                DomainId::new(1),
                IoVirtAddr::new(0x1000),
                0x4000_0000,
                0,
                DmaAccess::READ,
            ),
            Err(IommuError::Invalid)
        );
    }

    #[test]
    fn a_device_status_becomes_a_typed_error() {
        let (result, _) = exchange(S_NOENT, &[], |device| {
            device.attach(DomainId::new(1), EndpointId::new(0x18))
        });
        assert_eq!(result, Err(IommuError::NotFound));

        let (result, _) = exchange(S_INVAL, &[], |device| {
            device.detach(DomainId::new(1), EndpointId::new(0x18))
        });
        assert_eq!(result, Err(IommuError::Invalid));
    }

    #[test]
    fn probe_reports_the_doorbell_ranges_an_endpoint_keeps() {
        let mut properties = std::vec::Vec::new();
        properties.extend_from_slice(&PROBE_T_RESV_MEM.to_le_bytes());
        properties.extend_from_slice(&20_u16.to_le_bytes());
        properties.push(RESV_MEM_T_MSI);
        properties.extend_from_slice(&[0, 0, 0]);
        properties.extend_from_slice(&0xfee0_0000_u64.to_le_bytes());
        properties.extend_from_slice(&0xfee0_0fff_u64.to_le_bytes());
        properties.resize(PROBE_BYTES as usize, 0);

        let (result, request) = exchange(S_OK, &properties, |device| {
            let mut regions = [ReservedRegion {
                start: 0,
                bytes: 0,
                doorbell: false,
            }; MAX_RESERVED_REGIONS];
            device
                .probe(EndpointId::new(0x18), &mut regions)
                .map(|found| (found, regions[0]))
        });

        assert_eq!(request[0], T_PROBE);
        assert_eq!(&request[4..8], &0x18_u32.to_le_bytes());
        assert_eq!(
            result,
            Ok((
                1,
                ReservedRegion {
                    start: 0xfee0_0000,
                    bytes: 0x1000,
                    doorbell: true,
                }
            ))
        );
    }

    #[test]
    fn a_probe_reply_that_lists_nothing_reports_no_regions() {
        let mut regions = [ReservedRegion {
            start: 0,
            bytes: 0,
            doorbell: false,
        }; MAX_RESERVED_REGIONS];

        assert_eq!(decode_reserved_regions(&[0; 64], &mut regions), 0);
    }

    /// Every buffer goes straight back to the device, and it goes back
    /// paired with whatever identifier the ring gave it: the ring hands
    /// identifiers out in completion order, so a driver that assumed the
    /// old one would decode the next fault out of the wrong buffer.
    #[test]
    fn a_fault_event_is_counted_and_its_buffer_re_armed() {
        let device = device();
        {
            let mut state = device.event_queue.lock();
            let bytes = &mut state.buffers[..FAULT_BYTES];
            bytes[..4].copy_from_slice(&1_u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&0x18_u32.to_le_bytes());
            bytes[16..24].copy_from_slice(&0xdead_0000_u64.to_le_bytes());
            let written = u32::try_from(FAULT_BYTES).expect("the fault record fits");
            state.queue.device_complete(0, written);
        }

        device.drain_faults();

        assert_eq!(device.fault_count(), 1);
        let state = device.event_queue.lock();
        assert_eq!(
            state.queue.available_descriptors(),
            0,
            "every event buffer is back in the device's hands"
        );
        // The identifier the re-armed buffer was given points back at
        // the slot it actually occupies.
        assert_eq!(state.slot_for_token[0], 0);
    }
}
