//! `helios:system/input` for the component host.
//!
//! This is the whole of what a program sees of the machine's input
//! devices, and none of it touches the hardware. A claim moves one
//! device's claim word; from then on the events the kernel's drain
//! committed are read out of that device's queue, and the only call that
//! reaches the device at all is `set-led`, which travels the other way
//! on a queue of its own.
//!
//! The claims live in the instance's store, so they are single-owned:
//! the resource handles a program holds carry the device index and the
//! claim generation to check against, and nothing else. A handle that
//! outlived a release then names a device its instance no longer holds
//! and is refused, rather than being answered against whoever holds the
//! device now.
//!
//! # Concurrency contract
//!
//! Every call here runs on the task that owns the store, so the claims
//! need no lock. What is shared is the device's event queue, whose
//! reader arms its wait before it looks, and the indicator queue, which
//! carries its own synchronisation.

use alloc::string::String;
use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll};

use helios_hal::input::{AbsAxis, InputCapabilities};
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Accessor, Destination, HasSelf, Linker, Resource, StreamProducer, StreamReader,
    StreamResult, VecBuffer,
};

use crate::ComponentHostNetwork;
use crate::input::{DeviceIndex, InputEvents, InputLedSender, InputServiceError};
use crate::wasmtime_adapter::bindings::input::bindings::helios::system::input as input_wit;

use super::super::StoreData;

/// Register `helios:system/input` in a linker.
///
/// Every program linker gets it, for the reason the display interface
/// does: the claim is the capability, not the import. A program that
/// never claims a device learns nothing from having the import, and one
/// that claims on a machine with no input device is told `unavailable`.
pub(in crate::wasmtime_adapter::component_host) fn add_input_to_linker<CpuImpl, Net, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, Net, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    input_wit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(linker, |state| state)
}

/// A program's hold on one input device, as its store records it.
///
/// It carries the device and the generation and nothing else: the claim
/// itself is in the store's [`crate::InputOwnership`], which is what the
/// store's own drop releases.
pub struct InputDeviceHandle {
    index: DeviceIndex,
    generation: u64,
}

impl InputDeviceHandle {
    pub const fn new(index: DeviceIndex, generation: u64) -> Self {
        Self { index, generation }
    }

    pub const fn index(&self) -> DeviceIndex {
        self.index
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

const fn to_wit_error(error: InputServiceError) -> input_wit::Error {
    match error {
        InputServiceError::Unavailable => input_wit::Error::Unavailable,
        InputServiceError::NoSuchDevice => input_wit::Error::NoSuchDevice,
        // An instance that already holds every device the machine has
        // cannot be handed another one, and the answer it can act on is
        // the same as somebody else holding it: this device is not
        // available to you.
        InputServiceError::AlreadyClaimed | InputServiceError::TooManyDevices => {
            input_wit::Error::AlreadyClaimed
        }
        InputServiceError::NotClaimed => input_wit::Error::NotClaimed,
        // An owner task that has stopped serving is a machine on its way
        // down. There is nothing a program can do about it that it would
        // not also do about a device fault.
        InputServiceError::DeviceFault | InputServiceError::Closed => input_wit::Error::DeviceFault,
    }
}

fn to_wit_capabilities(capabilities: &InputCapabilities) -> input_wit::Capabilities {
    input_wit::Capabilities {
        name: capabilities.name().into(),
        ev_bits: capabilities
            .event_types()
            .iter()
            .map(|entry| input_wit::EventType {
                kind: entry.kind,
                codes: entry.codes.as_bytes().to_vec(),
            })
            .collect(),
        abs_info: capabilities
            .absolute_axes()
            .iter()
            .map(to_wit_abs_axis)
            .collect(),
    }
}

const fn to_wit_abs_axis(axis: &AbsAxis) -> input_wit::AbsAxis {
    input_wit::AbsAxis {
        code: axis.code,
        min: axis.info.min,
        max: axis.info.max,
        fuzz: axis.info.fuzz,
        flat: axis.info.flat,
        res: axis.info.res,
    }
}

impl<CpuImpl, Net, HostFs> StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    /// The indicator queue of one held device, checked against the
    /// generation the handle names.
    fn input_leds(
        &self,
        index: DeviceIndex,
        generation: u64,
    ) -> Result<InputLedSender, InputServiceError> {
        Ok(self.input.claim_ref(index, generation)?.leds())
    }
}

impl<CpuImpl, Net, HostFs> input_wit::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn available(&mut self) -> wasmtime::Result<Vec<input_wit::Capabilities>> {
        let Some(service) = self.runtime_state.input_service() else {
            return Ok(Vec::new());
        };
        Ok(service.available().map(to_wit_capabilities).collect())
    }

    fn claim(
        &mut self,
        name: String,
    ) -> wasmtime::Result<Result<Resource<InputDeviceHandle>, input_wit::Error>> {
        let Some(service) = self.runtime_state.input_service() else {
            return Ok(Err(input_wit::Error::Unavailable));
        };
        let (index, generation) = match self.input.claim(&service, &name) {
            Ok(claim) => (claim.index(), claim.generation()),
            Err(error) => {
                tracing::warn!(
                    target: "helios_kernel::input",
                    ?error,
                    device = name.as_str(),
                    "an instance was refused an input device"
                );
                return Ok(Err(to_wit_error(error)));
            }
        };
        let handle = self.table.push(InputDeviceHandle::new(index, generation))?;
        tracing::info!(
            target: "helios_kernel::input",
            device = name.as_str(),
            generation,
            "an instance took ownership of an input device"
        );
        Ok(Ok(handle))
    }
}

impl<CpuImpl, Net, HostFs> input_wit::HostDevice for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn capabilities(
        &mut self,
        handle: Resource<InputDeviceHandle>,
    ) -> wasmtime::Result<input_wit::Capabilities> {
        let handle = self.table.get(&handle)?;
        let (index, generation) = (handle.index(), handle.generation());
        let claim = self
            .input
            .claim_ref(index, generation)
            .map_err(|error| wasmtime::Error::msg(alloc::format!("{error}")))?;
        Ok(to_wit_capabilities(claim.capabilities()))
    }

    fn drop(&mut self, handle: Resource<InputDeviceHandle>) -> wasmtime::Result<()> {
        let handle = self.table.delete(handle)?;
        // Dropping the handle is a program saying it is done, and it
        // costs exactly what dying costs: the device goes back to the
        // kernel's own drain and whatever this owner never read is
        // thrown away.
        self.input.release(handle.index(), handle.generation());
        Ok(())
    }
}

/// The stream a program reads one device's events from.
struct InputEventStreamProducer {
    events: InputEvents,
}

impl Unpin for InputEventStreamProducer {}

impl<T: 'static> StreamProducer<T> for InputEventStreamProducer {
    type Item = input_wit::InputEvent;
    type Buffer = VecBuffer<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<'_, T>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        match self.events.poll_burst(cx) {
            Poll::Ready(burst) => {
                let events: Vec<Self::Item> = burst
                    .into_iter()
                    .map(|event| input_wit::InputEvent {
                        kind: event.kind,
                        code: event.code,
                        value: event.value,
                    })
                    .collect();
                destination.set_buffer(VecBuffer::from(events));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<CpuImpl, Net, HostFs, U> input_wit::HostDeviceWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, HostFs>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn events(
        mut accessor: Access<'_, U, Self>,
        handle: Resource<InputDeviceHandle>,
    ) -> wasmtime::Result<StreamReader<input_wit::InputEvent>> {
        // The store is given back between each step: a borrow of it held
        // across `StreamReader::new` would hold the store for as long as
        // the reader exists.
        let (index, generation) = {
            let named = accessor.get().table.get(&handle)?;
            (named.index(), named.generation())
        };
        // Armed here, before the reader is handed over: a report the
        // device publishes between this call and the program's first
        // read is one the program is owed.
        let events = accessor
            .get()
            .input
            .claim_ref(index, generation)
            .map_err(|error| wasmtime::Error::msg(alloc::format!("{error}")))?
            .events();
        StreamReader::new(&mut accessor, InputEventStreamProducer { events })
    }

    async fn set_led(
        accessor: &Accessor<U, Self>,
        handle: Resource<InputDeviceHandle>,
        code: u16,
        on: bool,
    ) -> wasmtime::Result<Result<(), input_wit::Error>> {
        let leds = accessor.with(|mut access| {
            let data = access.get();
            let handle = data.table.get(&handle)?;
            let (index, generation) = (handle.index(), handle.generation());
            Ok::<_, wasmtime::Error>(data.input_leds(index, generation))
        })?;
        let leds = match leds {
            Ok(leds) => leds,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        Ok(leds.set_led(code, on).await.map_err(to_wit_error))
    }
}
