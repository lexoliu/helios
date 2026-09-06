//! `helios:system/device` for the component host.
//!
//! This is the whole of what a user-mode driver sees of its hardware.
//! Everything it does here is bookkeeping the kernel owns — claiming a
//! device, mapping a region, pinning a buffer, acknowledging an
//! interrupt — and none of it is on the path a driver takes to a
//! register. Registers are reached by loading and storing in the
//! driver's own linear memory, at the offset `map-region` reports,
//! because the kernel mapped the device's frames there.
//!
//! The lease lives in the instance's store, so it is single-owned: the
//! resource handles a driver holds carry the device's name to check
//! against and nothing else. A handle that outlived a reclaim then
//! names a device its instance no longer holds and is refused, rather
//! than being answered against whatever the instance holds now.
//!
//! # Concurrency contract
//!
//! Every call here runs on the task that owns the store, so the lease
//! needs no lock. The interrupt stream is the one place another
//! processor is involved: the relay is written from interrupt context
//! and read here, and it carries its own synchronisation. The stream's
//! producer arms its wake-up through the relay before it inspects the
//! queue, so an interrupt taken between the inspection and the park is
//! not lost.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll};

use helios_hal::cpu::Cpu;
use helios_hal::device::{DeviceRegion, MemoryKind};
use helios_hal::vmm::VirtAddr;
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Destination, HasSelf, Linker, Resource, StreamProducer, StreamReader, StreamResult,
};

use crate::wasmtime_adapter::bindings::device::bindings::helios::system::device as device_wit;
use crate::{
    DeviceName, DmaBufferHandle, GrantError, GrantHandle, InterruptRelay, LinearMemory,
    NotifyWaiter,
};

use super::StoreData;

/// Register `helios:system/device` in a linker.
///
/// Every program linker gets it. The interface is a capability only in
/// the sense that the registry is: a claim by an instance the kernel
/// never gave a device to is refused, so an ordinary program importing
/// it learns there is no device for it and nothing else.
pub(super) fn add_device_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    device_wit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)
}

/// Record where an instance's linear memory landed.
///
/// Called once, right after the instance is built, on every component
/// the kernel runs. It is two numbers and no allocation, and doing it
/// here is what lets a later `claim` be an ordinary host call: the base
/// address a device window is measured from is already known by the
/// time a driver asks for one.
///
/// An instance whose component has no linear memory records nothing.
/// It could not see a register even if it were given one, and its claim
/// is refused when it makes one rather than here, where nobody asked.
pub(crate) fn record_linear_memory<CpuImpl, HostFs>(
    mut store: impl wasmtime::AsContextMut<Data = StoreData<CpuImpl, HostFs>>,
    instance: &wasmtime::component::Instance,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = instance.get_default_memory(store.as_context_mut()) else {
        return;
    };
    let data = memory.data(store.as_context());
    let base = VirtAddr::new(data.as_ptr() as usize);
    let bytes = data.len() as u64;
    let mut context = store.as_context_mut();
    let device = &mut context.data_mut().device;
    device.set_memory(LinearMemory {
        base,
        // Every linear memory the kernel builds gets the one reservation
        // profile the engine configures, and the compiled code depends
        // on it, so this is the machine's answer rather than this
        // instance's.
        reservation_bytes: helios_artifact::CWASM_MEMORY_RESERVATION,
    });
    device.note_growth(bytes);
}

fn to_wit_error(error: GrantError) -> device_wit::Error {
    match error {
        GrantError::NotFound | GrantError::NameTooLong => device_wit::Error::NotFound,
        GrantError::AlreadyClaimed => device_wit::Error::AlreadyClaimed,
        GrantError::NoSuchRegion | GrantError::NoSuchInterrupt => device_wit::Error::NoSuchIndex,
        GrantError::WindowExhausted => device_wit::Error::WindowExhausted,
        GrantError::BudgetExhausted => device_wit::Error::BudgetExhausted,
        GrantError::BadAlignment => device_wit::Error::BadAlignment,
        GrantError::Unreachable => device_wit::Error::Unreachable,
        // Everything left is a defect a driver cannot do anything
        // about: a grant that was built wrong, a registry that was
        // full, a backend that wired no platform surface. The driver is
        // told its mapping failed, and the kernel's log says which.
        GrantError::TooManyRegions
        | GrantError::TooManyInterrupts
        | GrantError::RegionNotFrameAligned
        | GrantError::RegistryFull
        | GrantError::DuplicateName
        | GrantError::PlatformUnavailable
        | GrantError::RegionAlreadyMapped
        | GrantError::AddressSpace(_) => device_wit::Error::MappingFailed,
    }
}

fn to_wit_region(region: &DeviceRegion) -> device_wit::Region {
    device_wit::Region {
        physical_address: region.physical.start,
        bytes: region.physical.bytes,
        kind: match region.attributes.kind {
            MemoryKind::Device => device_wit::MemoryKind::Device,
            MemoryKind::Normal => device_wit::MemoryKind::Normal,
        },
        writable: region.attributes.writable,
        prefetchable: region.attributes.prefetchable,
    }
}

impl<CpuImpl, HostFs> device_wit::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn claim(
        &mut self,
        name: String,
    ) -> wasmtime::Result<Result<Resource<GrantHandle>, device_wit::Error>> {
        let registry = self.runtime_state.device_grants();
        if let Err(error) = self.device.claim(registry, &name) {
            tracing::warn!(
                target: "helios_kernel::device",
                device = name.as_str(),
                ?error,
                "an instance was refused a device"
            );
            return Ok(Err(to_wit_error(error)));
        }
        let device = *self
            .device
            .lease()
            .expect("a successful claim leaves a lease")
            .grant()
            .name();
        let handle = self.table.push(GrantHandle::new(device))?;
        tracing::info!(
            target: "helios_kernel::device",
            device = name.as_str(),
            "an instance took ownership of a device"
        );
        Ok(Ok(handle))
    }

    fn available(&mut self) -> wasmtime::Result<Vec<String>> {
        Ok(self
            .runtime_state
            .device_grants()
            .devices()
            .iter()
            .map(|device| device.grant().name().as_str().to_string())
            .collect())
    }
}

impl<CpuImpl, HostFs> device_wit::HostGrant for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn name(&mut self, handle: Resource<GrantHandle>) -> wasmtime::Result<String> {
        Ok(self.table.get(&handle)?.device().as_str().to_string())
    }

    fn regions(
        &mut self,
        handle: Resource<GrantHandle>,
    ) -> wasmtime::Result<Vec<device_wit::Region>> {
        let device = *self.table.get(&handle)?.device();
        Ok(self
            .device
            .lease_for_mut(&device)
            .map(|lease| lease.grant().regions().iter().map(to_wit_region).collect())
            .unwrap_or_default())
    }

    fn map_region(
        &mut self,
        handle: Resource<GrantHandle>,
        index: u32,
    ) -> wasmtime::Result<Result<device_wit::Placement, device_wit::Error>> {
        let device = *self.table.get(&handle)?.device();
        let placed = self
            .device
            .lease_for_mut(&device)
            .and_then(|lease| lease.map_region(index as usize));
        Ok(placed
            .map(|region| device_wit::Placement {
                offset: region.offset,
                length: region.bytes,
            })
            .map_err(to_wit_error))
    }

    fn interrupt_count(&mut self, handle: Resource<GrantHandle>) -> wasmtime::Result<u32> {
        let device = *self.table.get(&handle)?.device();
        Ok(self
            .device
            .lease_for_mut(&device)
            .map(|lease| lease.grant().interrupts().len() as u32)
            .unwrap_or(0))
    }

    fn ack(
        &mut self,
        handle: Resource<GrantHandle>,
        index: u32,
    ) -> wasmtime::Result<Result<(), device_wit::Error>> {
        self.with_relay(handle, |relay| relay.ack(index as usize))
    }

    fn mask(
        &mut self,
        handle: Resource<GrantHandle>,
        index: u32,
    ) -> wasmtime::Result<Result<(), device_wit::Error>> {
        self.with_relay(handle, |relay| relay.mask(index as usize))
    }

    fn unmask(
        &mut self,
        handle: Resource<GrantHandle>,
        index: u32,
    ) -> wasmtime::Result<Result<(), device_wit::Error>> {
        self.with_relay(handle, |relay| relay.unmask(index as usize))
    }

    fn dma_alloc(
        &mut self,
        handle: Resource<GrantHandle>,
        bytes: u64,
        align: u64,
    ) -> wasmtime::Result<Result<Resource<DmaBufferHandle>, device_wit::Error>> {
        let device = *self.table.get(&handle)?.device();
        let buffer = self
            .device
            .lease_for_mut(&device)
            .and_then(|lease| lease.dma_alloc(bytes, align));
        let buffer = match buffer {
            Ok(buffer) => buffer,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        Ok(Ok(self.table.push(DmaBufferHandle::new(device, buffer))?))
    }

    fn confined(&mut self, handle: Resource<GrantHandle>) -> wasmtime::Result<bool> {
        let device = *self.table.get(&handle)?.device();
        Ok(self
            .device
            .lease_for_mut(&device)
            .is_ok_and(|lease| lease.grant().confinement().is_some()))
    }

    fn drop(&mut self, handle: Resource<GrantHandle>) -> wasmtime::Result<()> {
        let handle = self.table.delete(handle)?;
        // Dropping the handle is a driver saying it is done with the
        // device, and it costs exactly what dying costs: every source
        // masked, every region unmapped, every pin released, before the
        // device is offered to anyone else.
        if self
            .device
            .lease()
            .is_some_and(|lease| lease.grant().name() == handle.device())
        {
            self.device.release();
        }
        Ok(())
    }
}

impl<CpuImpl, HostFs> device_wit::HostDmaBuffer for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn offset(&mut self, handle: Resource<DmaBufferHandle>) -> wasmtime::Result<u64> {
        Ok(self.table.get(&handle)?.buffer().offset)
    }

    fn length(&mut self, handle: Resource<DmaBufferHandle>) -> wasmtime::Result<u64> {
        Ok(self.table.get(&handle)?.buffer().bytes)
    }

    fn physical_address(&mut self, handle: Resource<DmaBufferHandle>) -> wasmtime::Result<u64> {
        Ok(self.table.get(&handle)?.buffer().device_address)
    }

    fn drop(&mut self, handle: Resource<DmaBufferHandle>) -> wasmtime::Result<()> {
        // The pin belongs to the lease and is released with it, so
        // dropping the handle only forgets the descriptor. A driver
        // builds its rings once and holds them for as long as it holds
        // the device.
        self.table.delete(handle)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    /// Run `act` against the relay of the device `handle` names.
    fn with_relay<Act>(
        &mut self,
        handle: Resource<GrantHandle>,
        act: Act,
    ) -> wasmtime::Result<Result<(), device_wit::Error>>
    where
        Act: FnOnce(&InterruptRelay) -> Result<(), GrantError>,
    {
        let device = *self.table.get(&handle)?.device();
        let relay = match self.device.lease_for_mut(&device) {
            Ok(lease) => lease.relay(),
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        Ok(act(relay).map_err(to_wit_error))
    }
}

/// The stream a driver reads its device's interrupts from.
///
/// It holds no reference to the relay: the lease is in the store, and
/// the producer is polled with the store in hand, so it reaches the
/// relay through it. That is what keeps the lease single-owned and lets
/// a reclaim end the stream rather than race it — once the lease is
/// gone the stream reports the device is gone too.
struct InterruptStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    device: DeviceName,
    waiter: Option<NotifyWaiter>,
}

impl<T, CpuImpl, HostFs> Unpin for InterruptStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<T, CpuImpl, HostFs> InterruptStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    const fn new(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        device: DeviceName,
    ) -> Self {
        Self {
            getter,
            device,
            waiter: None,
        }
    }
}

impl<T, CpuImpl, HostFs> StreamProducer<T> for InterruptStreamProducer<T, CpuImpl, HostFs>
where
    T: 'static,
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = device_wit::InterruptEvent;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'_, T>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        let producer = &mut *self;
        let device = producer.device;
        let data = (producer.getter)(store.data_mut());
        let Some(lease) = data
            .device
            .lease()
            .filter(|lease| lease.grant().name() == &device)
        else {
            // The device was taken back. A driver still reading sees the
            // stream end, which is what it would see if it had been
            // killed, and is the only honest answer: the interrupts are
            // no longer its to hear.
            return Poll::Ready(Ok(StreamResult::Dropped));
        };
        let relay = lease.relay();
        let waiter = producer.waiter.get_or_insert_with(|| relay.waiter());
        match relay.poll_event(cx, waiter) {
            Poll::Ready(event) => {
                destination.set_buffer(Some(device_wit::InterruptEvent {
                    index: event.index,
                    sequence: event.sequence,
                }));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<CpuImpl, HostFs, U> device_wit::HostGrantWithStore<U> for HasSelf<StoreData<CpuImpl, HostFs>>
where
    U: 'static,
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn interrupts(
        mut accessor: Access<'_, U, Self>,
        handle: Resource<GrantHandle>,
    ) -> wasmtime::Result<StreamReader<device_wit::InterruptEvent>> {
        let getter = accessor.getter();
        let device = *accessor.get().table.get(&handle)?.device();
        StreamReader::new(&mut accessor, InterruptStreamProducer::new(getter, device))
    }
}
