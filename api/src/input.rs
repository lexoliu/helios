//! The machine's input devices, and the events a program reads from
//! them.
//!
//! Exactly one program holds a device at a time. [`Device::claim`] takes
//! one by name; dropping what that returns — or dying — hands it back to
//! the kernel, and the next caller may have it. A program that wants a
//! whole desktop claims every device [`available`] lists.
//!
//! The vocabulary is evdev's, unchanged: an event is a `(kind, code,
//! value)` triple and a burst that belongs to one physical moment is
//! closed by a `SYN_REPORT`. [`codes`] is the same table the kernel
//! reads them by, so a program names a key the way every other operating
//! system does.
//!
//! # What a slow reader loses
//!
//! The kernel relays each device's events through a queue as deep as the
//! driver's own ring, so a program that stops reading cannot stall the
//! machine's input. What it loses is a whole report at a time, never
//! half of one: a pointer never keeps one axis of a move whose other
//! half it dropped. Reports lost that way are counted on
//! `helios:system/stats`.

use std::string::String;
use std::vec::Vec;

use thiserror::Error;

use crate::bindings::helios::system::input as raw;
use crate::wit_bindgen::StreamReader;

pub use crate::bindings::helios::system::input::{AbsAxis, Capabilities, EventType, InputEvent};

pub use helios_hal::input::code_name;
/// The evdev code tables, as the kernel and its drivers read them.
///
/// One table, shared: a program that carried its own copy would name
/// `KEY_ENTER` by a number the kernel had since renumbered.
pub use helios_hal::input::codes;

/// Why an input request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum InputError {
    #[error("this machine has no input device")]
    Unavailable,
    #[error("this machine has no input device of that name")]
    NoSuchDevice,
    #[error("another program already holds every input device of that name")]
    AlreadyClaimed,
    #[error("this program does not hold that input device")]
    NotClaimed,
    #[error("the input device faulted")]
    DeviceFault,
}

impl From<raw::Error> for InputError {
    fn from(error: raw::Error) -> Self {
        match error {
            raw::Error::Unavailable => Self::Unavailable,
            raw::Error::NoSuchDevice => Self::NoSuchDevice,
            raw::Error::AlreadyClaimed => Self::AlreadyClaimed,
            raw::Error::NotClaimed => Self::NotClaimed,
            raw::Error::DeviceFault => Self::DeviceFault,
        }
    }
}

/// What every input device on this machine says about itself, whether or
/// not somebody holds it.
pub fn available() -> Vec<Capabilities> {
    raw::available()
}

/// This program's hold on one input device.
pub struct Device {
    raw: raw::Device,
}

impl Device {
    /// Take exclusive ownership of the device `name` names.
    ///
    /// The second caller is refused rather than queued: a program
    /// waiting for a keyboard another program holds is a provisioning
    /// mistake, not a shortage. On a machine with two devices of that
    /// name, claiming twice yields both.
    pub fn claim(name: &str) -> Result<Self, InputError> {
        raw::claim(name)
            .map(|raw| Self { raw })
            .map_err(InputError::from)
    }

    /// Claim every device this machine has, in the order the kernel
    /// brought them up.
    ///
    /// A device somebody else already holds is left with them rather
    /// than failing the whole call: a compositor that comes up beside a
    /// program holding the tablet still wants the keyboard.
    pub fn claim_all() -> Vec<Self> {
        available()
            .iter()
            .filter_map(|capabilities| Self::claim(&capabilities.name).ok())
            .collect()
    }

    /// What this device says about itself.
    pub fn capabilities(&self) -> Capabilities {
        self.raw.capabilities()
    }

    /// The name this device presents itself by.
    pub fn name(&self) -> String {
        self.raw.capabilities().name
    }

    /// Every event this device reports, from now on.
    ///
    /// `SYN_REPORT` is in the stream like any other event: the frame
    /// boundary belongs to whoever is interpreting the events.
    pub fn events(&self) -> StreamReader<InputEvent> {
        self.raw.events()
    }

    /// Turn one of this device's indicators on or off.
    ///
    /// `code` is an `LED_*` code the device reports for `EV_LED`.
    pub async fn set_led(&self, code: u16, on: bool) -> Result<(), InputError> {
        self.raw.set_led(code, on).await.map_err(InputError::from)
    }
}

/// The `EV_*` name of an event's type, where the generated revision
/// defines one.
pub fn event_type_name(event: InputEvent) -> Option<&'static str> {
    codes::event_type_name(event.kind)
}

/// The name of an event's code within its type's namespace.
///
/// `None` for a code the generated revision does not define, and for an
/// event type whose codes have no namespace of their own.
pub fn event_code_name(event: InputEvent) -> Option<&'static str> {
    code_name(event.kind, event.code)
}

/// Whether this event closes an input frame.
///
/// Everything reported before it and after the previous one happened at
/// the same instant as far as the device is concerned, which is what a
/// consumer has to honour: a pointer's two axes arrive as separate
/// events and acting on the first alone draws a diagonal as a staircase.
pub const fn ends_frame(event: InputEvent) -> bool {
    event.kind == codes::EV_SYN && event.code == codes::SYN_REPORT
}
