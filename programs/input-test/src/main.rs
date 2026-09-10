//! `input-test`: reads the machine's input devices and says what they
//! reported.
//!
//! It is the guest side of the input path's acceptance evidence. It
//! claims every device `helios:system/input` lists, reads each one's
//! event stream, and prints one line per event in evdev's own
//! vocabulary — `SYN_REPORT` included, because the frame boundary is
//! what says which events happened at the same instant.
//!
//! Every step prints a line, because a capture that comes back with no
//! events has to be told apart from a program that never claimed a
//! device.
//!
//! One task per device, all of them feeding one channel, so the lines
//! come out in one order rather than interleaved mid-line. A device's
//! own events keep the order the device produced them in; two devices
//! are ordered by when the machine delivered them, which is what a host
//! driving a scripted keyboard and pointer is asserting on.

use std::env;
use std::time::Duration;

use helios_api::channel::{Sender, bounded};
use helios_api::input::{Device, InputError, InputEvent, event_code_name, event_type_name};
use helios_api::task::{sleep, spawn};
use thiserror::Error;

/// Lines that may be waiting to be printed before a reader waits for
/// room.
///
/// A reader that is this far ahead of the printer is a reader whose
/// device is producing faster than a serial line carries, and making it
/// wait is the backpressure that keeps the program's own memory bounded.
const LINE_QUEUE_DEPTH: usize = 64;

/// Events one read of a device's stream may take at once.
///
/// The kernel's queue per device is 64 events deep, so a read this size
/// empties it in one pass whatever the device did.
const READ_EVENTS: usize = 64;

#[derive(Debug, Error)]
enum InputTestError {
    #[error("usage: input-test [--seconds <n>] [--events <n>] [--device <name>]")]
    Usage,
    #[error("--{option} needs a number, not {value:?}")]
    NotANumber { option: &'static str, value: String },
    #[error("this machine has no input device to read")]
    NoDevices,
    #[error("the input service refused: {0}")]
    Input(#[from] InputError),
}

struct Options {
    /// How long to read for. The events a host injects arrive while this
    /// runs, so it is what bounds the whole run.
    seconds: u64,
    /// Stop early once this many events have been printed. Absent means
    /// read for the whole `--seconds`.
    events: Option<usize>,
    /// Read only the device of this name. Absent means every device the
    /// machine has.
    device: Option<String>,
}

/// Long enough for a host to send a scripted key press and a pointer
/// move with a wait between them, and short enough that a lane that
/// fails still ends.
const DEFAULT_SECONDS: u64 = 10;

fn parse_options() -> Result<Options, InputTestError> {
    let mut options = Options {
        seconds: DEFAULT_SECONDS,
        events: None,
        device: None,
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--seconds" => {
                let value = arguments.next().ok_or(InputTestError::Usage)?;
                options.seconds = value.parse().map_err(|_| InputTestError::NotANumber {
                    option: "seconds",
                    value,
                })?;
            }
            "--events" => {
                let value = arguments.next().ok_or(InputTestError::Usage)?;
                options.events = Some(value.parse().map_err(|_| InputTestError::NotANumber {
                    option: "events",
                    value,
                })?);
            }
            "--device" => options.device = Some(arguments.next().ok_or(InputTestError::Usage)?),
            _ => return Err(InputTestError::Usage),
        }
    }
    Ok(options)
}

/// The `EV_*` name of an event's type, or its number where the generated
/// evdev revision defines no name for it.
///
/// The number rather than a placeholder: a line that says `unnamed` says
/// nothing a reader can look up, and a device that reports a type this
/// kernel does not know about is exactly the case worth reading.
fn kind_label(event: InputEvent) -> String {
    event_type_name(event).map_or_else(|| format!("{:#06x}", event.kind), String::from)
}

/// The name of an event's code within its type's namespace, or its
/// number where there is none.
fn code_label(event: InputEvent) -> String {
    event_code_name(event).map_or_else(|| format!("{:#06x}", event.code), String::from)
}

/// Read one device for as long as its stream lasts, rendering each event
/// onto the shared line queue.
async fn read_device(device: Device, lines: Sender<String>) {
    let name = device.name();
    let mut events = device.events();
    loop {
        let (result, burst) = events.read(Vec::with_capacity(READ_EVENTS)).await;
        for event in burst {
            let line = format!(
                "input-test:event device={name} kind={} code={} value={}",
                kind_label(event),
                code_label(event),
                event.value
            );
            if lines.send(line).await.is_err() {
                // The printer has stopped, which is the run ending. The
                // device goes back to the kernel when this task's
                // handle drops.
                return;
            }
        }
        if helios_api::stream_closed(result) {
            println!("input-test:stream-ended device={name}");
            return;
        }
    }
}

#[helios_api::main]
async fn main() -> Result<(), InputTestError> {
    let options = parse_options()?;

    let claimed = match &options.device {
        Some(name) => vec![Device::claim(name)?],
        None => Device::claim_all(),
    };
    if claimed.is_empty() {
        return Err(InputTestError::NoDevices);
    }
    for device in &claimed {
        let capabilities = device.capabilities();
        println!(
            "input-test:claimed device={} types={} axes={}",
            capabilities.name,
            capabilities.ev_bits.len(),
            capabilities.abs_info.len()
        );
    }
    println!("input-test:reading devices={}", claimed.len());

    let (lines, printed) = bounded::<String>(LINE_QUEUE_DEPTH);
    for device in claimed {
        let lines = lines.clone();
        spawn(async move {
            read_device(device, lines).await;
        });
    }
    // The deadline closes the queue rather than cancelling the readers:
    // a reader cancelled mid-burst would leave events printed for one
    // device and not the other, and what this program is evidence of is
    // the order the machine delivered them in.
    let deadline = lines.clone();
    let seconds = options.seconds;
    spawn(async move {
        sleep(Duration::from_secs(seconds)).await;
        deadline.close();
    });
    // The program's own handle on the sending end goes now, so the queue
    // closes when the readers are done even before the deadline.
    drop(lines);

    let mut count = 0_usize;
    while let Ok(line) = printed.recv().await {
        println!("{line}");
        count += 1;
        if options.events.is_some_and(|target| count >= target) {
            break;
        }
    }
    println!("input-test:done events={count}");
    Ok(())
}
