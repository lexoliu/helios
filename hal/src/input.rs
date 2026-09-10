//! The human-input device contract, in evdev's vocabulary.
//!
//! Input hardware has one event encoding that every operating system
//! and every virtual machine already speaks: Linux's evdev. An event is
//! a `(type, code, value)` triple, a device describes itself by which
//! codes it can report for each type, and a burst of triples that
//! belong to one physical moment — a pointer that moved in both axes,
//! a key that went down while a modifier was held — is closed by a
//! `SYN_REPORT`. virtio-input carries exactly that, minus the
//! timestamp, so this module reproduces the vocabulary rather than
//! translating it into one of this kernel's own: a translation would
//! have to be undone by every consumer, and there is no fact about
//! input a Helios-specific model could carry that evdev does not.
//!
//! The numbers themselves live in [`codes`], generated from a named
//! revision of Linux's `input-event-codes.h`.
//!
//! Frames are the consumer's business, not the driver's. A device
//! reports its triples in order and marks the end of each frame; a
//! driver that buffered a frame would add latency to the one thing on
//! this path that a person can feel, so [`InputDevice::next_event`]
//! hands each event over as it arrives and [`InputEvent::ends_frame`]
//! is what closes one.
//!
//! # SMP contract
//!
//! An [`InputDevice`] is shared. Every method takes `&self` and may be
//! called from any processor. `next_event` parks on the device's own
//! notification rather than polling, and the implementation serialises
//! access to its rings internally; `set_led` travels the other way, on
//! a queue of its own, and never waits behind a pending event.
//! [`InputCapabilities`] are read once, by one owner, while the device
//! is brought up, and are immutable afterwards, so `capabilities`
//! hands back a reference and takes no lock.

use core::fmt::{self, Write as _};
use core::future::Future;

use arrayvec::{ArrayString, ArrayVec};

use crate::io::{IoError, IoResult};

pub mod codes;

/// Longest device name or serial evdev carries.
///
/// virtio-input publishes both through a configuration payload of this
/// size, and the evdev `EVIOCGNAME` buffer conventional userspace uses
/// is no larger.
pub const MAX_DEVICE_STRING_BYTES: usize = 128;

/// Largest code bitmap one event type needs.
///
/// A bitmap has one bit per code in its namespace. The widest namespace
/// is `KEY_*`, whose [`codes::KEY_CNT`] codes need 96 bytes; 128 is the
/// payload a virtio-input configuration block carries a bitmap in, and
/// is therefore what a device may legitimately publish.
pub const MAX_CODE_BITMAP_BYTES: usize = 128;

/// Event types one device may declare.
///
/// evdev numbers its types up to [`codes::EV_MAX`], so a device cannot
/// declare more than this however unusual it is.
pub const MAX_EVENT_TYPES: usize = codes::EV_CNT as usize;

/// Absolute axes one device may report.
///
/// Bounded the same way: evdev numbers absolute axes up to
/// [`codes::ABS_MAX`].
pub const MAX_ABS_AXES: usize = codes::ABS_CNT as usize;

/// One input event: a Linux `struct input_event` without its
/// timestamp, which is what a device puts on the wire.
///
/// `kind` is an `EV_*` type, `code` names a code within that type's
/// namespace, and `value` is what the code reports — a key's state, a
/// relative displacement, an absolute position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    pub const fn new(kind: u16, code: u16, value: i32) -> Self {
        Self { kind, code, value }
    }

    /// Whether this event closes an input frame.
    ///
    /// Everything reported before it and after the previous one
    /// happened at the same instant as far as the device is concerned,
    /// which is what a consumer has to honour: a pointer's two axes
    /// arrive as separate events and moving on the first alone draws a
    /// diagonal as a staircase.
    pub const fn ends_frame(self) -> bool {
        self.kind == codes::EV_SYN && self.code == codes::SYN_REPORT
    }

    /// The `EV_*` name of this event's type, where the generated
    /// revision defines one.
    pub const fn type_name(self) -> Option<&'static str> {
        codes::event_type_name(self.kind)
    }

    /// The name of this event's code within its type's namespace.
    ///
    /// `None` for a code the generated revision does not define, and
    /// for an event type whose codes have no namespace of their own
    /// (force feedback addresses effect ids, not codes).
    pub const fn code_name(self) -> Option<&'static str> {
        code_name(self.kind, self.code)
    }
}

/// The name of `code` within the namespace `kind` names.
pub const fn code_name(kind: u16, code: u16) -> Option<&'static str> {
    match kind {
        codes::EV_SYN => codes::synchronization_name(code),
        codes::EV_KEY => codes::key_name(code),
        codes::EV_REL => codes::relative_axis_name(code),
        codes::EV_ABS => codes::absolute_axis_name(code),
        codes::EV_MSC => codes::misc_name(code),
        codes::EV_SW => codes::switch_name(code),
        codes::EV_LED => codes::led_name(code),
        codes::EV_SND => codes::sound_name(code),
        codes::EV_REP => codes::repeat_name(code),
        _ => None,
    }
}

/// The four numbers evdev identifies a device model by, as
/// `struct input_id` orders them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceIds {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

/// What an absolute axis reports over, as `struct input_absinfo`
/// describes it.
///
/// `fuzz` is the noise the device's own filtering leaves behind, `flat`
/// the dead zone around the resting position, and `res` the units per
/// millimetre (or per radian, for a rotational axis) — zero where the
/// device does not know its own scale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AbsInfo {
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub res: i32,
}

/// One absolute axis a device reports, and its range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsAxis {
    pub code: u16,
    pub info: AbsInfo,
}

/// The set of codes a device reports for one event type, as the bit per
/// code the device publishes.
///
/// The length is the device's own: it publishes as many bytes as its
/// highest code needs, and a code past the end is simply one this
/// device does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeBitmap {
    bytes: [u8; MAX_CODE_BITMAP_BYTES],
    len: usize,
}

impl CodeBitmap {
    /// The empty bitmap: a device that reports no code of this type.
    pub const fn empty() -> Self {
        Self {
            bytes: [0; MAX_CODE_BITMAP_BYTES],
            len: 0,
        }
    }

    /// Takes the bitmap a device published.
    ///
    /// A bitmap longer than [`MAX_CODE_BITMAP_BYTES`] is refused rather
    /// than truncated: the codes past the cut are ones the device says
    /// it can report, and silently dropping them would leave a consumer
    /// certain a key does not exist.
    pub fn from_bytes(bytes: &[u8]) -> IoResult<Self> {
        if bytes.len() > MAX_CODE_BITMAP_BYTES {
            return Err(IoError::InvalidDeviceConfig(
                "input device published a code bitmap longer than an evdev namespace",
            ));
        }
        let mut bitmap = Self::empty();
        bitmap.bytes[..bytes.len()].copy_from_slice(bytes);
        bitmap.len = bytes.len();
        Ok(bitmap)
    }

    /// The bytes the device published, as it published them.
    ///
    /// Bit `b` of byte `n` is code `n * 8 + b`, which is the encoding
    /// evdev itself uses and the one a consumer outside this kernel is
    /// handed: rebuilding the bitmap from [`Self::codes`] would cost a
    /// pass over every code in the namespace to say the same thing.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// Whether the device reports `code`.
    pub const fn contains(&self, code: u16) -> bool {
        let index = code as usize / 8;
        if index >= self.len {
            return false;
        }
        self.bytes[index] & (1 << (code % 8)) != 0
    }

    /// Whether the device reports no code at all for this type.
    pub fn is_empty(&self) -> bool {
        self.bytes[..self.len].iter().all(|byte| *byte == 0)
    }

    /// Every code the device reports, in ascending order.
    pub fn codes(&self) -> impl Iterator<Item = u16> + '_ {
        self.bytes[..self.len]
            .iter()
            .enumerate()
            .flat_map(|(index, byte)| {
                (0..8u16).filter_map(move |bit| {
                    (byte & (1 << bit) != 0).then_some(index as u16 * 8 + bit)
                })
            })
    }
}

/// One event type a device declares, and the codes it reports for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventTypeCodes {
    pub kind: u16,
    pub codes: CodeBitmap,
}

/// Everything a device says about itself.
///
/// Built once, while the device is brought up, and immutable
/// afterwards. The fields are private because the invariants are: an
/// event type appears at most once, and so does an absolute axis, which
/// is what lets a consumer trust a single lookup.
#[derive(Clone, Debug)]
pub struct InputCapabilities {
    name: ArrayString<MAX_DEVICE_STRING_BYTES>,
    serial: ArrayString<MAX_DEVICE_STRING_BYTES>,
    ids: DeviceIds,
    properties: CodeBitmap,
    types: ArrayVec<EventTypeCodes, MAX_EVENT_TYPES>,
    axes: ArrayVec<AbsAxis, MAX_ABS_AXES>,
}

impl InputCapabilities {
    /// A device that has said nothing about itself yet.
    pub fn new() -> Self {
        Self {
            name: ArrayString::new(),
            serial: ArrayString::new(),
            ids: DeviceIds::default(),
            properties: CodeBitmap::empty(),
            types: ArrayVec::new(),
            axes: ArrayVec::new(),
        }
    }

    /// Records the device's name.
    ///
    /// The bytes are the device's, so they are checked here rather than
    /// trusted: a name that is not UTF-8, or that is longer than evdev
    /// carries, is a configuration this kernel cannot describe and the
    /// device is refused while it is still named by its address.
    pub fn set_name(&mut self, bytes: &[u8]) -> IoResult<()> {
        self.name = device_string(bytes)?;
        Ok(())
    }

    /// Records the device's serial number. Same rules as the name; an
    /// absent serial is an empty string, not a failure.
    pub fn set_serial(&mut self, bytes: &[u8]) -> IoResult<()> {
        self.serial = device_string(bytes)?;
        Ok(())
    }

    pub fn set_ids(&mut self, ids: DeviceIds) {
        self.ids = ids;
    }

    /// Records the `INPUT_PROP_*` bitmap: what kind of thing the device
    /// is, beyond which codes it reports.
    pub fn set_properties(&mut self, properties: CodeBitmap) {
        self.properties = properties;
    }

    /// Records the codes the device reports for one event type.
    pub fn push_event_type(&mut self, kind: u16, codes: CodeBitmap) -> IoResult<()> {
        if self.codes_for(kind).is_some() {
            return Err(IoError::InvalidDeviceConfig(
                "input device declared the same event type twice",
            ));
        }
        self.types
            .try_push(EventTypeCodes { kind, codes })
            .map_err(|_| {
                IoError::InvalidDeviceConfig(
                    "input device declared more event types than evdev defines",
                )
            })
    }

    /// Records one absolute axis and the range it reports over.
    ///
    /// Axes arrive in ascending code order, because that is the order
    /// the driver walks the `EV_ABS` bitmap in, and keeping them so is
    /// what makes the boot line's axis list reproducible.
    pub fn push_absolute_axis(&mut self, code: u16, info: AbsInfo) -> IoResult<()> {
        if self.axes.last().is_some_and(|last| last.code >= code) {
            return Err(IoError::InvalidDeviceConfig(
                "input device reported its absolute axes out of order",
            ));
        }
        self.axes.try_push(AbsAxis { code, info }).map_err(|_| {
            IoError::InvalidDeviceConfig("input device reported more absolute axes than evdev has")
        })
    }

    /// The device's name, as it presents itself.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The device's serial number, empty where it has none.
    pub fn serial(&self) -> &str {
        &self.serial
    }

    pub fn ids(&self) -> DeviceIds {
        self.ids
    }

    /// The `INPUT_PROP_*` bitmap.
    pub fn properties(&self) -> &CodeBitmap {
        &self.properties
    }

    /// Every event type the device declares, in the order it declared
    /// them.
    pub fn event_types(&self) -> &[EventTypeCodes] {
        &self.types
    }

    /// The codes the device reports for `kind`, or `None` when it
    /// declares no such type.
    pub fn codes_for(&self, kind: u16) -> Option<&CodeBitmap> {
        self.types
            .iter()
            .find(|entry| entry.kind == kind)
            .map(|entry| &entry.codes)
    }

    /// Whether the device reports `code` for event type `kind`.
    pub fn reports(&self, kind: u16, code: u16) -> bool {
        self.codes_for(kind)
            .is_some_and(|bitmap| bitmap.contains(code))
    }

    /// Every absolute axis the device reports, in ascending code order.
    pub fn absolute_axes(&self) -> &[AbsAxis] {
        &self.axes
    }

    /// The range one absolute axis reports over.
    pub fn absolute_axis(&self, code: u16) -> Option<AbsInfo> {
        self.axes
            .iter()
            .find(|axis| axis.code == code)
            .map(|axis| axis.info)
    }

    /// The event types this device carries, rendered the way a boot
    /// line names them: `KEY,ABS`, the `EV_` prefix dropped because
    /// every entry in the list would carry it.
    ///
    /// Every declared type is named, an empty bitmap included: a device
    /// declares `EV_REP` with no codes at all, and what that says is
    /// that it repeats held keys, which is a fact about the device and
    /// not an absence.
    pub fn event_type_list(&self) -> impl fmt::Display + '_ {
        EventTypeList(self)
    }

    /// The absolute axes this device reports, rendered the way a boot
    /// line names them: `x:0..32767,y:0..32767`.
    pub fn absolute_axis_list(&self) -> impl fmt::Display + '_ {
        AbsAxisList(self)
    }
}

impl Default for InputCapabilities {
    fn default() -> Self {
        Self::new()
    }
}

/// Checks one device-published string and stores it.
fn device_string(bytes: &[u8]) -> IoResult<ArrayString<MAX_DEVICE_STRING_BYTES>> {
    let text = core::str::from_utf8(bytes).map_err(|_| {
        IoError::InvalidDeviceConfig("input device published a name or serial that is not UTF-8")
    })?;
    ArrayString::from(text).map_err(|_| {
        IoError::InvalidDeviceConfig(
            "input device published a name or serial longer than evdev carries",
        )
    })
}

struct EventTypeList<'a>(&'a InputCapabilities);

impl fmt::Display for EventTypeList<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let types = self.0.event_types();
        if types.is_empty() {
            formatter.write_str("none")?;
        }
        for (position, entry) in types.iter().enumerate() {
            if position != 0 {
                formatter.write_str(",")?;
            }
            match codes::event_type_name(entry.kind) {
                // Every name in this namespace starts `EV_`, and the
                // list is already known to hold event types.
                Some(name) => formatter.write_str(name.trim_start_matches("EV_"))?,
                None => write!(formatter, "{:#x}", entry.kind)?,
            }
        }
        Ok(())
    }
}

struct AbsAxisList<'a>(&'a InputCapabilities);

impl fmt::Display for AbsAxisList<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let axes = self.0.absolute_axes();
        if axes.is_empty() {
            formatter.write_str("none")?;
        }
        for (position, axis) in axes.iter().enumerate() {
            if position != 0 {
                formatter.write_str(",")?;
            }
            match codes::absolute_axis_name(axis.code) {
                // `ABS_X` reads as `x`: the prefix is what the list is
                // already known to hold, and the lower case is how
                // evdev's own tooling spells an axis.
                Some(name) => {
                    for byte in name.trim_start_matches("ABS_").bytes() {
                        formatter.write_char(char::from(byte.to_ascii_lowercase()))?;
                    }
                }
                None => write!(formatter, "{:#x}", axis.code)?,
            }
            write!(formatter, ":{}..{}", axis.info.min, axis.info.max)?;
        }
        Ok(())
    }
}

/// A human-input device: a keyboard, a pointer, a tablet, or anything
/// else that reports evdev events.
pub trait InputDevice: Send + Sync + 'static {
    /// What the device says about itself. Read once at bring-up and
    /// immutable afterwards.
    fn capabilities(&self) -> &InputCapabilities;

    /// The next event the device reported, waiting for one when the
    /// device has said nothing since the last call.
    ///
    /// Events arrive in the order the device produced them, one at a
    /// time, `SYN_REPORT` included: the frame boundary is an event like
    /// any other and belongs to whoever is interpreting the stream.
    fn next_event(&self) -> impl Future<Output = IoResult<InputEvent>> + Send + '_;

    /// Turns one of the device's indicators on or off.
    ///
    /// `code` is an `LED_*` code the device reports for `EV_LED`; this
    /// is the one direction in which events travel towards the device.
    fn set_led(&self, code: u16, on: bool) -> impl Future<Output = IoResult<()>> + Send + '_;
}

/// A shared handle to an input device is an input device.
///
/// The kernel holds one device through several owners at once — the
/// interrupt route dispatches to it, and whatever consumes its events
/// reads from it — and both need the whole contract rather than a
/// borrow with a lifetime.
impl<Device: InputDevice + ?Sized> InputDevice for alloc::sync::Arc<Device> {
    fn capabilities(&self) -> &InputCapabilities {
        Device::capabilities(self)
    }

    fn next_event(&self) -> impl Future<Output = IoResult<InputEvent>> + Send + '_ {
        Device::next_event(self)
    }

    fn set_led(&self, code: u16, on: bool) -> impl Future<Output = IoResult<()>> + Send + '_ {
        Device::set_led(self, code, on)
    }
}

#[cfg(test)]
mod tests {
    use super::{AbsInfo, CodeBitmap, InputCapabilities, InputEvent, codes};
    use alloc::format;
    use alloc::vec::Vec;

    /// The tablet QEMU presents: absolute axes over the whole reporting
    /// range, and the three pointer buttons.
    fn tablet() -> InputCapabilities {
        let mut capabilities = InputCapabilities::new();
        capabilities
            .set_name(b"QEMU Virtio Tablet")
            .expect("an ASCII name is accepted");
        capabilities
            .push_event_type(codes::EV_SYN, bitmap(&[codes::SYN_REPORT]))
            .expect("a first declaration of a type is accepted");
        capabilities
            .push_event_type(codes::EV_KEY, bitmap(&[codes::BTN_LEFT, codes::BTN_RIGHT]))
            .expect("a first declaration of a type is accepted");
        capabilities
            .push_event_type(codes::EV_ABS, bitmap(&[codes::ABS_X, codes::ABS_Y]))
            .expect("a first declaration of a type is accepted");
        for code in [codes::ABS_X, codes::ABS_Y] {
            capabilities
                .push_absolute_axis(
                    code,
                    AbsInfo {
                        min: 0,
                        max: 32767,
                        ..AbsInfo::default()
                    },
                )
                .expect("axes are pushed in ascending order");
        }
        capabilities
    }

    fn bitmap(codes: &[u16]) -> CodeBitmap {
        let longest = codes.iter().copied().max().unwrap_or(0);
        let mut bytes = alloc::vec![0_u8; usize::from(longest) / 8 + 1];
        for code in codes {
            bytes[usize::from(*code) / 8] |= 1 << (code % 8);
        }
        CodeBitmap::from_bytes(&bytes).expect("a bitmap of evdev codes fits")
    }

    /// The line a backend prints when a device comes up has to name the
    /// device the way a person recognises it, and the tablet's ranges
    /// are what says it is a tablet rather than a mouse.
    #[test]
    fn the_boot_line_names_the_types_and_the_axis_ranges() {
        let capabilities = tablet();

        assert_eq!(
            format!(
                "{} ev={} abs={}",
                capabilities.name(),
                capabilities.event_type_list(),
                capabilities.absolute_axis_list()
            ),
            "QEMU Virtio Tablet ev=SYN,KEY,ABS abs=x:0..32767,y:0..32767"
        );
    }

    /// A keyboard has no absolute axis at all, and the line has to say
    /// so rather than trailing off after `abs=`.
    #[test]
    fn a_device_without_absolute_axes_says_so() {
        let mut capabilities = InputCapabilities::new();
        capabilities
            .push_event_type(codes::EV_KEY, bitmap(&[codes::KEY_A]))
            .expect("a first declaration of a type is accepted");

        assert_eq!(format!("{}", capabilities.absolute_axis_list()), "none");
        assert_eq!(format!("{}", capabilities.event_type_list()), "KEY");
    }

    /// A type with no codes is still a type the device declared: a
    /// keyboard declares `EV_REP` exactly that way, and what it means
    /// is that the device repeats held keys itself.
    #[test]
    fn an_event_type_without_codes_is_still_named() {
        let mut capabilities = InputCapabilities::new();
        capabilities
            .push_event_type(codes::EV_KEY, bitmap(&[codes::KEY_A]))
            .expect("a first declaration of a type is accepted");
        capabilities
            .push_event_type(codes::EV_REP, CodeBitmap::empty())
            .expect("a first declaration of a type is accepted");

        assert_eq!(format!("{}", capabilities.event_type_list()), "KEY,REP");
    }

    /// A device that declared nothing at all has to say so, rather than
    /// leaving the boot line's `ev=` with nothing after it.
    #[test]
    fn a_device_that_declares_no_event_type_says_so() {
        assert_eq!(
            format!("{}", InputCapabilities::new().event_type_list()),
            "none"
        );
    }

    #[test]
    fn a_bitmap_reports_exactly_the_codes_its_bits_name() {
        let keys = bitmap(&[codes::KEY_A, codes::KEY_Z, codes::BTN_LEFT]);

        assert!(keys.contains(codes::KEY_A));
        assert!(keys.contains(codes::BTN_LEFT));
        assert!(!keys.contains(codes::KEY_B));
        // A code past the end of the published bytes is one the device
        // does not have, not an out-of-range access.
        assert!(!keys.contains(codes::KEY_MAX));
        assert_eq!(
            keys.codes().collect::<Vec<_>>(),
            alloc::vec![codes::KEY_A, codes::KEY_Z, codes::BTN_LEFT]
        );
    }

    /// The same event type twice would leave a lookup answering with
    /// whichever copy came first, so it is refused.
    #[test]
    fn a_repeated_event_type_is_refused() {
        let mut capabilities = InputCapabilities::new();
        capabilities
            .push_event_type(codes::EV_KEY, bitmap(&[codes::KEY_A]))
            .expect("a first declaration of a type is accepted");

        capabilities
            .push_event_type(codes::EV_KEY, bitmap(&[codes::KEY_B]))
            .expect_err("a second declaration of the same type is a broken device");
    }

    #[test]
    fn a_bitmap_longer_than_an_evdev_namespace_is_refused() {
        CodeBitmap::from_bytes(&alloc::vec![0_u8; super::MAX_CODE_BITMAP_BYTES + 1])
            .expect_err("a bitmap past the end of the namespace is a broken device");
    }

    /// Only `SYN_REPORT` closes a frame; `SYN_DROPPED` says the device
    /// lost events, and treating it as a frame end would present half a
    /// frame as a whole one.
    #[test]
    fn only_syn_report_ends_a_frame() {
        assert!(InputEvent::new(codes::EV_SYN, codes::SYN_REPORT, 0).ends_frame());
        assert!(!InputEvent::new(codes::EV_SYN, codes::SYN_DROPPED, 0).ends_frame());
        assert!(!InputEvent::new(codes::EV_KEY, codes::KEY_A, 1).ends_frame());
    }

    #[test]
    fn an_event_names_itself_in_evdev_terms() {
        let event = InputEvent::new(codes::EV_ABS, codes::ABS_X, 128);

        assert_eq!(event.type_name(), Some("EV_ABS"));
        assert_eq!(event.code_name(), Some("ABS_X"));
        assert_eq!(
            InputEvent::new(codes::EV_KEY, codes::BTN_LEFT, 1).code_name(),
            Some("BTN_LEFT")
        );
    }
}
