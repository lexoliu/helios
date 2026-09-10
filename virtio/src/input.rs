//! virtio-input driver: keyboards, pointers and absolute tablets.
//!
//! The device is the simplest shape a virtio device comes in. Two
//! queues carry evdev events in the two directions — `eventq` from the
//! device, `statusq` towards it — and everything else the driver needs
//! to know arrives through a configuration register file rather than a
//! command protocol: the driver writes a selector and a sub-selector,
//! reads back how many bytes that pair answers with, and then reads
//! them (virtio 1.2 §5.8.5). A pair the device does not answer reports
//! a size of zero, and that is also how the driver discovers which
//! event types the device supports: it asks for each type's code
//! bitmap in turn and a non-zero size is the yes.
//!
//! The register file is stateful — the selector belongs to whoever
//! wrote it last — so it is read exactly once, on the bring-up path,
//! by the single processor that programs the device, and never
//! afterwards. [`VirtioInputDevice::capabilities`] hands out what was
//! read; nothing in the driver touches configuration space again.
//!
//! # Receive memory
//!
//! The event ring *is* the buffer pool. One eight-byte
//! `virtio_input_event` is allocated per descriptor at bring-up and
//! recycled for the lifetime of the device: a used buffer is decoded
//! into a value and reposted before the queue lock is released, so
//! nothing on the receive path allocates and no reader can pin a
//! buffer the device needs back (AGENTS.md §3.1). An event is 8 bytes
//! and its consumer wants a `(type, code, value)` triple, so copying
//! it out costs nothing an owning buffer would save.
//!
//! # Concurrency contract
//!
//! `next_event` serialises on the event ring's own async mutex, parks
//! on the device interrupt when the ring is empty, and never holds the
//! lock across an await. `set_led` goes the other way on the status
//! queue, through the shared `submit_chain`/`await_completion` pair, so
//! several tasks may have a status event in flight at once and neither
//! direction waits behind the other. Both are safe to call from any
//! processor. `handle_interrupt` runs in interrupt context: it
//! acknowledges the device and wakes waiters, and does nothing else.

use alloc::boxed::Box;
use alloc::vec;
use async_lock::Mutex as AsyncMutex;

use helios_hal::input::{
    AbsInfo, CodeBitmap, DeviceIds, InputCapabilities, InputDevice, InputEvent, codes,
};
use helios_hal::io::{IoError, IoResult};

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate};
use crate::inflight::{InFlight, await_completion, submit_chain};
use crate::notify::Notify;
use crate::queue::{VirtQueue, negotiated_queue_size};
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

/// The queue the device reports events on (virtio 1.2 §5.8.2).
const EVENT_QUEUE_INDEX: u16 = 0;
/// The queue the driver sends events to the device on.
const STATUS_QUEUE_INDEX: u16 = 1;

/// Depth the driver asks for on the event queue, and therefore how many
/// events the device may have reported before the guest has taken any
/// of them back.
///
/// A frame of pointer motion is three events and a key press two, so
/// this holds a burst of roughly twenty frames — more than a person
/// generates between two turns of the executor, which is what keeps the
/// device from ever finding the ring full.
const EVENT_QUEUE_SIZE: u16 = 64;
/// The status queue carries one event per indicator change.
const STATUS_QUEUE_SIZE: u16 = 8;

/// A buffer in either direction is a single descriptor: one event.
const EVENT_CHAIN_LIMIT: u16 = 1;
const STATUS_CHAIN_LIMIT: u16 = 1;

/// `struct virtio_input_event`: a little-endian type, code and value.
const EVENT_BYTES: usize = 8;

/// Byte offsets in `struct virtio_input_config` (virtio 1.2 §5.8.5).
const CFG_SELECT: usize = 0;
const CFG_SUBSEL: usize = 1;
const CFG_SIZE: usize = 2;
const CFG_PAYLOAD: usize = 8;

/// The payload window the register file answers in.
const CFG_PAYLOAD_BYTES: usize = 128;

/// `VIRTIO_INPUT_CFG_UNSET`: the selector that answers nothing. The
/// driver leaves it here once bring-up is done, so a stray read of the
/// register file cannot come back looking like an answer.
const CFG_UNSET: u8 = 0x00;
/// `VIRTIO_INPUT_CFG_ID_NAME`: the device's name, NUL-terminated.
const CFG_ID_NAME: u8 = 0x01;
/// `VIRTIO_INPUT_CFG_ID_SERIAL`: its serial number, likewise.
const CFG_ID_SERIAL: u8 = 0x02;
/// `VIRTIO_INPUT_CFG_ID_DEVIDS`: `struct virtio_input_devids`.
const CFG_ID_DEVIDS: u8 = 0x03;
/// `VIRTIO_INPUT_CFG_PROP_BITS`: the `INPUT_PROP_*` bitmap.
const CFG_PROP_BITS: u8 = 0x10;
/// `VIRTIO_INPUT_CFG_EV_BITS`: the code bitmap for the event type named
/// by the sub-selector.
const CFG_EV_BITS: u8 = 0x11;
/// `VIRTIO_INPUT_CFG_ABS_INFO`: `struct virtio_input_absinfo` for the
/// absolute axis named by the sub-selector.
const CFG_ABS_INFO: u8 = 0x12;

/// `struct virtio_input_devids`: four little-endian 16-bit ids.
const DEVIDS_BYTES: usize = 8;
/// `struct virtio_input_absinfo`: five little-endian 32-bit fields.
const ABSINFO_BYTES: usize = 20;

/// The event ring together with the buffer pool that backs it.
///
/// Slot bookkeeping lives beside the queue rather than behind a lock of
/// its own because every path that touches it already holds the event
/// queue: a completion is drained, its event decoded, and the slot
/// reposted before the lock is released.
struct EventRing<T: VirtioTransport> {
    queue: VirtQueue<T>,
    /// One `EVENT_BYTES` slot per descriptor, in one allocation:
    /// the pool is fixed at bring-up and every slot has the same size,
    /// so there is nothing for a per-slot allocation to express.
    slots: Box<[u8]>,
    /// Which slot each outstanding descriptor identifier carries.
    slot_for_token: Box<[u16]>,
}

pub struct VirtioInputDevice<T: VirtioTransport> {
    transport: T,
    capabilities: InputCapabilities,
    events: AsyncMutex<EventRing<T>>,
    status_queue: AsyncMutex<VirtQueue<T>>,
    status_inflight: InFlight<{ STATUS_QUEUE_SIZE as usize }>,
    interrupts: Notify,
    features: NegotiatedFeatures,
}

impl<T: VirtioTransport> VirtioInputDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Input {
            return Err(IoError::Unsupported);
        }

        let features = negotiate(&transport, RING_FEATURES)?;

        let event_size = negotiated_queue_size(&transport, EVENT_QUEUE_INDEX, EVENT_QUEUE_SIZE)?;
        let status_size = negotiated_queue_size(&transport, STATUS_QUEUE_INDEX, STATUS_QUEUE_SIZE)?;

        let mut event_queue = VirtQueue::new(
            &transport,
            EVENT_QUEUE_INDEX,
            event_size,
            EVENT_CHAIN_LIMIT,
            features,
        )?;
        let status_queue = VirtQueue::new(
            &transport,
            STATUS_QUEUE_INDEX,
            status_size,
            STATUS_CHAIN_LIMIT,
            features,
        )?;

        let mut slots = vec![0_u8; usize::from(event_size) * EVENT_BYTES].into_boxed_slice();
        let mut slot_for_token = vec![0_u16; usize::from(event_size)].into_boxed_slice();
        for index in 0..usize::from(event_size) {
            let slot = &mut slots[index * EVENT_BYTES..(index + 1) * EVENT_BYTES];
            let token = event_queue.submit_output_deferred(&transport, slot)?;
            slot_for_token[usize::from(token)] =
                u16::try_from(index).map_err(|_| IoError::DeviceFault)?;
        }
        event_queue.publish();

        // The register file is read while this processor is still the
        // only one that can reach the device, and before `DRIVER_OK`
        // lets it report anything.
        let capabilities = read_capabilities(&transport)?;

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );
        event_queue.notify(&transport);

        Ok(Self {
            transport,
            capabilities,
            events: AsyncMutex::new(EventRing {
                queue: event_queue,
                slots,
                slot_for_token,
            }),
            status_queue: AsyncMutex::new(status_queue),
            status_inflight: InFlight::new(),
            interrupts: Notify::new(),
            features,
        })
    }

    /// The feature set this device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    /// Interrupt handlers should only acknowledge the device and wake
    /// waiters.
    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        self.interrupts.notify_all();
    }

    /// Takes one event out of the ring, if the device has published
    /// one, and reposts the slot it came in.
    fn take_event(state: &mut EventRing<T>, transport: &T) -> IoResult<Option<InputEvent>> {
        let EventRing {
            queue,
            slots,
            slot_for_token,
        } = state;
        let Some((token, used_len)) = queue.pop_used_with_len() else {
            return Ok(None);
        };
        let index = usize::from(
            *slot_for_token
                .get(usize::from(token))
                .ok_or(IoError::DeviceFault)?,
        );
        let slot = slots
            .get_mut(index * EVENT_BYTES..(index + 1) * EVENT_BYTES)
            .ok_or(IoError::DeviceFault)?;
        // The slot goes back to the device whatever the event turns out
        // to be: a fault that also leaked a buffer would shrink the ring
        // on top of dropping the event.
        let decoded = decode_event(slot, used_len);
        let token = queue.submit_output_deferred(transport, slot)?;
        slot_for_token[usize::from(token)] =
            u16::try_from(index).map_err(|_| IoError::DeviceFault)?;
        queue.publish();
        queue.notify(transport);
        decoded.map(Some)
    }
}

impl<T: VirtioTransport> InputDevice for VirtioInputDevice<T> {
    fn capabilities(&self) -> &InputCapabilities {
        &self.capabilities
    }

    async fn next_event(&self) -> IoResult<InputEvent> {
        loop {
            // Armed before the ring is drained: an event the device
            // publishes in between belongs to this wait, not to the
            // next interrupt.
            let notified = self.interrupts.notified();
            {
                let mut state = self.events.lock().await;
                if let Some(event) = Self::take_event(&mut state, &self.transport)? {
                    return Ok(event);
                }
            }
            notified.await;
        }
    }

    async fn set_led(&self, code: u16, on: bool) -> IoResult<()> {
        if !self.capabilities.reports(codes::EV_LED, code) {
            return Err(IoError::Unsupported);
        }
        let event = encode_event(InputEvent::new(codes::EV_LED, code, i32::from(on)));
        let token = submit_chain(
            &self.status_inflight,
            &self.status_queue,
            &self.transport,
            &[&event],
            &mut [],
        )
        .await?;
        await_completion(&self.status_inflight, &self.status_queue, token, || {
            self.interrupts.notified()
        })
        .await;
        Ok(())
    }
}

impl<T: VirtioTransport> Drop for VirtioInputDevice<T> {
    fn drop(&mut self) {
        self.events.get_mut().queue.shutdown(&self.transport);
        self.status_queue.get_mut().shutdown(&self.transport);
    }
}

/// Decodes one used event slot.
fn decode_event(slot: &[u8], used_len: u32) -> IoResult<InputEvent> {
    // The device writes whole events and nothing else; a short or long
    // one means the ring and the device disagree about the buffer, and
    // guessing which half is real would report input that never
    // happened.
    if usize::try_from(used_len).map_err(|_| IoError::DeviceFault)? != EVENT_BYTES {
        return Err(IoError::DeviceFault);
    }
    let bytes: &[u8; EVENT_BYTES] = slot
        .get(..EVENT_BYTES)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(IoError::DeviceFault)?;
    Ok(InputEvent {
        kind: u16::from_le_bytes([bytes[0], bytes[1]]),
        code: u16::from_le_bytes([bytes[2], bytes[3]]),
        value: i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    })
}

/// Encodes one event into its eight wire bytes.
fn encode_event(event: InputEvent) -> [u8; EVENT_BYTES] {
    let mut bytes = [0_u8; EVENT_BYTES];
    bytes[0..2].copy_from_slice(&event.kind.to_le_bytes());
    bytes[2..4].copy_from_slice(&event.code.to_le_bytes());
    bytes[4..8].copy_from_slice(&event.value.to_le_bytes());
    bytes
}

/// Asks the configuration register file one question.
///
/// Returns how many bytes the device answered with, written into
/// `payload`. Zero means the device does not answer this pair at all,
/// which is the specification's way of saying "not supported" and is
/// how the event types are discovered.
fn read_config_block<T: VirtioTransport>(
    transport: &T,
    select: u8,
    subsel: u8,
    payload: &mut [u8; CFG_PAYLOAD_BYTES],
) -> IoResult<usize> {
    transport.write_config_u8(CFG_SELECT, select);
    transport.write_config_u8(CFG_SUBSEL, subsel);
    let size = usize::from(transport.read_config_u8(CFG_SIZE));
    if size > CFG_PAYLOAD_BYTES {
        return Err(IoError::InvalidDeviceConfig(
            "virtio-input device answered a configuration query with more bytes than the \
             register file holds",
        ));
    }
    for (index, byte) in payload[..size].iter_mut().enumerate() {
        *byte = transport.read_config_u8(CFG_PAYLOAD + index);
    }
    Ok(size)
}

/// Reads everything the device says about itself.
///
/// The device answers one question at a time and the selector is
/// device-wide state, so this runs on the bring-up path, under the
/// single owner, and is the only code that ever writes it.
fn read_capabilities<T: VirtioTransport>(transport: &T) -> IoResult<InputCapabilities> {
    let mut capabilities = InputCapabilities::new();
    let mut payload = [0_u8; CFG_PAYLOAD_BYTES];

    let len = read_config_block(transport, CFG_ID_NAME, 0, &mut payload)?;
    capabilities.set_name(trim_terminator(&payload[..len]))?;
    let len = read_config_block(transport, CFG_ID_SERIAL, 0, &mut payload)?;
    capabilities.set_serial(trim_terminator(&payload[..len]))?;

    let len = read_config_block(transport, CFG_ID_DEVIDS, 0, &mut payload)?;
    if len >= DEVIDS_BYTES {
        capabilities.set_ids(decode_devids(&payload));
    }
    // A device that publishes no ids leaves them zero: evdev shows the
    // same, and nothing here dispatches on them.

    let len = read_config_block(transport, CFG_PROP_BITS, 0, &mut payload)?;
    capabilities.set_properties(CodeBitmap::from_bytes(&payload[..len])?);

    // There is no query that lists the event types: the driver asks for
    // each type's bitmap and a non-zero answer is the declaration
    // (virtio 1.2 §5.8.5.2). `EV_SYN` is not among them — every device
    // closes its frames, so there is nothing to ask.
    for kind in 1..codes::EV_CNT {
        let subsel = u8::try_from(kind).map_err(|_| IoError::DeviceFault)?;
        let len = read_config_block(transport, CFG_EV_BITS, subsel, &mut payload)?;
        if len == 0 {
            continue;
        }
        capabilities.push_event_type(kind, CodeBitmap::from_bytes(&payload[..len])?)?;
    }

    // The axes are exactly the codes the `EV_ABS` bitmap names, so the
    // ranges are read for those and no others. Collected before the
    // loop because the bitmap borrows the capabilities being filled.
    let axes: Option<CodeBitmap> = capabilities.codes_for(codes::EV_ABS).copied();
    if let Some(axes) = axes {
        for axis in axes.codes() {
            let subsel = u8::try_from(axis).map_err(|_| IoError::DeviceFault)?;
            let len = read_config_block(transport, CFG_ABS_INFO, subsel, &mut payload)?;
            if len < ABSINFO_BYTES {
                // The device named the axis in its bitmap and then
                // refused to say what it reports over, so a consumer
                // would have to invent a range. That is a broken
                // device, named here while it still can be.
                return Err(IoError::InvalidDeviceConfig(
                    "virtio-input device reports an absolute axis it will not describe",
                ));
            }
            capabilities.push_absolute_axis(axis, decode_absinfo(&payload))?;
        }
    }

    transport.write_config_u8(CFG_SELECT, CFG_UNSET);
    transport.write_config_u8(CFG_SUBSEL, 0);
    Ok(capabilities)
}

/// The bytes of a device string without the NUL the device pads it
/// with: the size it reports counts the terminator, and evdev names are
/// text.
fn trim_terminator(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    &bytes[..end]
}

fn decode_devids(payload: &[u8; CFG_PAYLOAD_BYTES]) -> DeviceIds {
    DeviceIds {
        bustype: u16::from_le_bytes([payload[0], payload[1]]),
        vendor: u16::from_le_bytes([payload[2], payload[3]]),
        product: u16::from_le_bytes([payload[4], payload[5]]),
        version: u16::from_le_bytes([payload[6], payload[7]]),
    }
}

fn decode_absinfo(payload: &[u8; CFG_PAYLOAD_BYTES]) -> AbsInfo {
    let field = |index: usize| {
        let start = index * 4;
        i32::from_le_bytes([
            payload[start],
            payload[start + 1],
            payload[start + 2],
            payload[start + 3],
        ])
    };
    AbsInfo {
        min: field(0),
        max: field(1),
        fuzz: field(2),
        flat: field(3),
        res: field(4),
    }
}

/// Announces one input device on the line a backend's boot log carries.
///
/// It lives here rather than in each backend because the three of them
/// would otherwise each render the same capabilities their own way, and
/// a boot line is evidence: it has to read the same whichever machine
/// produced it (AGENTS.md §1).
pub(crate) fn report_input_online<T: VirtioTransport>(
    device: &VirtioInputDevice<T>,
    transport: &str,
) {
    let capabilities = device.capabilities();
    tracing::info!(
        "virtio-input online transport={transport} name=\"{}\" ev={} abs={}",
        capabilities.name(),
        capabilities.event_type_list(),
        capabilities.absolute_axis_list()
    );
}

#[cfg(test)]
mod tests {
    use super::{
        CFG_ABS_INFO, CFG_ID_DEVIDS, CFG_ID_NAME, CFG_ID_SERIAL, CFG_PROP_BITS, DeviceType,
        EVENT_BYTES, IoError, VirtioInputDevice, encode_event,
    };
    use crate::testing::{FakeTransport, FakeTransportConfig};
    use crate::transport::VirtioFeatures;
    use alloc::format;
    use alloc::vec::Vec;
    use core::pin::pin;
    use futures_lite::future::{block_on, poll_once};
    use helios_hal::input::{AbsInfo, InputDevice, InputEvent, codes};

    /// `VIRTIO_INPUT_CFG_EV_BITS`, as the tests spell a sub-selected
    /// query.
    const CFG_EV_BITS: u8 = 0x11;

    /// Builds the configuration register file QEMU's `virtio-keyboard`
    /// presents: a name, the device ids it shares with the other HID
    /// devices, autorepeat declared with no codes at all, the three
    /// keyboard indicators, and a key bitmap.
    fn keyboard() -> FakeTransport {
        let transport = transport();
        transport.set_config_block(CFG_ID_NAME, 0, b"QEMU Virtio Keyboard\0");
        transport.set_config_block(CFG_ID_DEVIDS, 0, &devids(0x0006, 0x0627, 0x0001, 0x0001));
        transport.set_config_block(
            CFG_EV_BITS,
            ev(codes::EV_KEY),
            &bitmap(&[codes::KEY_A, 0x54]),
        );
        transport.set_config_block(CFG_EV_BITS, ev(codes::EV_REP), &[0]);
        transport.set_config_block(
            CFG_EV_BITS,
            ev(codes::EV_LED),
            &bitmap(&[codes::LED_NUML, codes::LED_CAPSL, codes::LED_SCROLLL]),
        );
        transport
    }

    /// QEMU's `virtio-tablet`: pointer buttons, a wheel, and the two
    /// absolute axes whose ranges are what make it a tablet.
    fn tablet() -> FakeTransport {
        let transport = transport();
        transport.set_config_block(CFG_ID_NAME, 0, b"QEMU Virtio Tablet\0");
        transport.set_config_block(CFG_ID_DEVIDS, 0, &devids(0x0006, 0x0627, 0x0003, 0x0002));
        transport.set_config_block(
            CFG_EV_BITS,
            ev(codes::EV_KEY),
            &bitmap(&[codes::BTN_LEFT, codes::BTN_RIGHT, codes::BTN_MIDDLE]),
        );
        transport.set_config_block(CFG_EV_BITS, ev(codes::EV_REL), &bitmap(&[codes::REL_WHEEL]));
        transport.set_config_block(
            CFG_EV_BITS,
            ev(codes::EV_ABS),
            &bitmap(&[codes::ABS_X, codes::ABS_Y]),
        );
        for axis in [codes::ABS_X, codes::ABS_Y] {
            transport.set_config_block(CFG_ABS_INFO, ev(axis), &absinfo(0, 32767));
        }
        transport
    }

    /// QEMU's `virtio-mouse`: the same buttons, relative axes, and no
    /// absolute axis at all.
    fn mouse() -> FakeTransport {
        let transport = transport();
        transport.set_config_block(CFG_ID_NAME, 0, b"QEMU Virtio Mouse\0");
        transport.set_config_block(CFG_ID_SERIAL, 0, b"mouse-0\0");
        transport.set_config_block(CFG_ID_DEVIDS, 0, &devids(0x0006, 0x0627, 0x0002, 0x0001));
        transport.set_config_block(
            CFG_EV_BITS,
            ev(codes::EV_KEY),
            &bitmap(&[codes::BTN_LEFT, codes::BTN_RIGHT, codes::BTN_MIDDLE]),
        );
        transport.set_config_block(
            CFG_EV_BITS,
            ev(codes::EV_REL),
            &bitmap(&[codes::REL_X, codes::REL_Y, codes::REL_WHEEL]),
        );
        transport
    }

    fn transport() -> FakeTransport {
        FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Input,
            offered_features: VirtioFeatures::VERSION_1.bits(),
            queue_size: 8,
            supports_queue_reset: false,
            absent_queues: &[],
        })
    }

    fn ev(kind: u16) -> u8 {
        u8::try_from(kind).expect("every evdev type and axis fits a sub-selector")
    }

    fn bitmap(codes: &[u16]) -> Vec<u8> {
        let longest = codes.iter().copied().max().unwrap_or(0);
        let mut bytes = alloc::vec![0_u8; usize::from(longest) / 8 + 1];
        for code in codes {
            bytes[usize::from(*code) / 8] |= 1 << (code % 8);
        }
        bytes
    }

    fn devids(bustype: u16, vendor: u16, product: u16, version: u16) -> Vec<u8> {
        [bustype, vendor, product, version]
            .iter()
            .flat_map(|field| field.to_le_bytes())
            .collect()
    }

    fn absinfo(min: i32, max: i32) -> Vec<u8> {
        [min, max, 0, 0, 0]
            .iter()
            .flat_map(|field| field.to_le_bytes())
            .collect()
    }

    /// Plays the device: writes `event` into the slot descriptor `token`
    /// carries, publishes the completion, and raises the interrupt.
    fn report(device: &VirtioInputDevice<FakeTransport>, token: u16, event: InputEvent) {
        let mut state = device
            .events
            .try_lock()
            .expect("a parked driver does not hold the event queue lock");
        let index = usize::from(state.slot_for_token[usize::from(token)]);
        state.slots[index * EVENT_BYTES..(index + 1) * EVENT_BYTES]
            .copy_from_slice(&encode_event(event));
        state.queue.device_complete(token, EVENT_BYTES as u32);
        drop(state);
        device.handle_interrupt();
    }

    #[test]
    fn a_wrong_device_type_is_rejected() {
        let rejected = VirtioInputDevice::new(FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Block,
            ..FakeTransportConfig::default()
        }))
        .err();
        assert_eq!(rejected, Some(IoError::Unsupported));
    }

    /// The register file is the only thing that says what a device is,
    /// and the three QEMU devices differ only in what it answers.
    #[test]
    fn the_register_file_describes_the_qemu_keyboard() {
        let device = VirtioInputDevice::new(keyboard()).expect("a keyboard initializes");
        let capabilities = device.capabilities();

        assert_eq!(capabilities.name(), "QEMU Virtio Keyboard");
        assert_eq!(capabilities.serial(), "");
        assert_eq!(capabilities.ids().vendor, 0x0627);
        assert_eq!(capabilities.ids().product, 0x0001);
        assert!(capabilities.reports(codes::EV_KEY, codes::KEY_A));
        assert!(capabilities.reports(codes::EV_LED, codes::LED_CAPSL));
        assert!(!capabilities.reports(codes::EV_ABS, codes::ABS_X));
        // Autorepeat is declared with an empty bitmap; the declaration
        // itself is the fact, so it has to survive into the line.
        assert_eq!(format!("{}", capabilities.event_type_list()), "KEY,LED,REP");
        assert_eq!(format!("{}", capabilities.absolute_axis_list()), "none");
    }

    #[test]
    fn the_register_file_describes_the_qemu_tablet() {
        let device = VirtioInputDevice::new(tablet()).expect("a tablet initializes");
        let capabilities = device.capabilities();

        assert_eq!(capabilities.name(), "QEMU Virtio Tablet");
        assert_eq!(
            capabilities.absolute_axis(codes::ABS_X),
            Some(AbsInfo {
                min: 0,
                max: 32767,
                fuzz: 0,
                flat: 0,
                res: 0,
            })
        );
        assert_eq!(format!("{}", capabilities.event_type_list()), "KEY,REL,ABS");
        assert_eq!(
            format!("{}", capabilities.absolute_axis_list()),
            "x:0..32767,y:0..32767"
        );
    }

    #[test]
    fn the_register_file_describes_the_qemu_mouse() {
        let device = VirtioInputDevice::new(mouse()).expect("a mouse initializes");
        let capabilities = device.capabilities();

        assert_eq!(capabilities.name(), "QEMU Virtio Mouse");
        assert_eq!(capabilities.serial(), "mouse-0");
        assert!(capabilities.reports(codes::EV_REL, codes::REL_X));
        assert_eq!(format!("{}", capabilities.event_type_list()), "KEY,REL");
        assert_eq!(format!("{}", capabilities.absolute_axis_list()), "none");
    }

    /// An axis the device names in its bitmap and then will not
    /// describe leaves a consumer inventing a range, so bring-up fails
    /// instead.
    #[test]
    fn an_absolute_axis_without_a_range_is_refused() {
        let transport = transport();
        transport.set_config_block(CFG_ID_NAME, 0, b"broken tablet\0");
        transport.set_config_block(CFG_EV_BITS, ev(codes::EV_ABS), &bitmap(&[codes::ABS_X]));

        assert_eq!(
            VirtioInputDevice::new(transport).err(),
            Some(IoError::InvalidDeviceConfig(
                "virtio-input device reports an absolute axis it will not describe"
            ))
        );
    }

    /// A device property bitmap says what kind of thing the device is;
    /// a touchpad's `INPUT_PROP_POINTER` is what tells a compositor not
    /// to draw its coordinates directly on the screen.
    #[test]
    fn device_properties_are_read() {
        let transport = transport();
        transport.set_config_block(CFG_ID_NAME, 0, b"touchpad\0");
        transport.set_config_block(CFG_PROP_BITS, 0, &bitmap(&[codes::INPUT_PROP_POINTER]));
        let device = VirtioInputDevice::new(transport).expect("a touchpad initializes");

        assert!(
            device
                .capabilities()
                .properties()
                .contains(codes::INPUT_PROP_POINTER)
        );
        assert!(
            !device
                .capabilities()
                .properties()
                .contains(codes::INPUT_PROP_DIRECT)
        );
    }

    /// A frame of pointer motion is three events and the driver hands
    /// them over one at a time, `SYN_REPORT` included: the boundary is
    /// the consumer's to act on, and a driver that swallowed it would
    /// leave nothing to act on.
    #[test]
    fn events_arrive_one_at_a_time_across_a_frame() {
        let device = VirtioInputDevice::new(tablet()).expect("a tablet initializes");
        let frame = [
            InputEvent::new(codes::EV_ABS, codes::ABS_X, 1024),
            InputEvent::new(codes::EV_ABS, codes::ABS_Y, 2048),
            InputEvent::new(codes::EV_SYN, codes::SYN_REPORT, 0),
        ];
        for (token, event) in frame.iter().enumerate() {
            report(&device, token as u16, *event);
        }

        for expected in frame {
            let mut next = pin!(device.next_event());
            assert_eq!(block_on(poll_once(next.as_mut())), Some(Ok(expected)));
        }
        assert!(
            block_on(poll_once(pin!(device.next_event()))).is_none(),
            "a drained ring parks instead of inventing an event"
        );
        assert!(
            frame
                .iter()
                .filter(|event| event.ends_frame())
                .count()
                .eq(&1)
        );
    }

    /// The ring is the whole buffer pool, so a slot that was read has
    /// to be back with the device before the reader returns — otherwise
    /// a device that reports more events than the ring is deep would
    /// stall on a guest that is keeping up.
    #[test]
    fn a_read_slot_goes_straight_back_to_the_device() {
        let device = VirtioInputDevice::new(keyboard()).expect("a keyboard initializes");
        let key = InputEvent::new(codes::EV_KEY, codes::KEY_A, 1);
        report(&device, 0, key);

        assert_eq!(
            block_on(poll_once(pin!(device.next_event()))),
            Some(Ok(key))
        );
        // Descriptor 0 is available again, which is only true if the
        // driver reposted it: the ring was full of receive buffers.
        report(&device, 0, key);
        assert_eq!(
            block_on(poll_once(pin!(device.next_event()))),
            Some(Ok(key))
        );
    }

    /// An indicator the device does not report is not one the driver
    /// may switch on: the status queue would carry an event the device
    /// has to ignore, and the caller would believe the light is on.
    #[test]
    fn an_led_the_device_does_not_have_is_refused() {
        let device = VirtioInputDevice::new(keyboard()).expect("a keyboard initializes");

        assert_eq!(
            block_on(poll_once(pin!(device.set_led(codes::LED_KANA, true)))),
            Some(Err(IoError::Unsupported))
        );
    }

    #[test]
    fn an_event_round_trips_through_its_wire_bytes() {
        let event = InputEvent::new(codes::EV_ABS, codes::ABS_Y, -3);
        let bytes = encode_event(event);

        assert_eq!(super::decode_event(&bytes, EVENT_BYTES as u32), Ok(event));
        assert_eq!(
            super::decode_event(&bytes, 4),
            Err(IoError::DeviceFault),
            "a partial event is a device that disagrees with its own ring"
        );
    }
}
