//! A minimal QMP client for the knobs the inspector exposes.
//!
//! QEMU's machine protocol is a line-oriented JSON stream: a greeting,
//! a capability handshake, then one response object per command, with
//! asynchronous events interleaved between them. Only the commands the
//! inspector actually drives live here — the point is to spare a
//! developer a hand-written `socat` session, not to grow a second QEMU
//! front end.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How long a single command waits for its response.
///
/// A guest that is handing memory back keeps QEMU's vCPU thread busy
/// discarding pages, and the monitor only runs between those bursts, so
/// a reply can take a while. It is still bounded: a caller that cannot
/// get an answer says so rather than hanging the session.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

/// Why a QMP command did not produce an answer.
///
/// The socket faults, the protocol faults and QEMU's own refusal are
/// separate variants because they point at different things: the host's
/// socket, this client's framing, and the machine's state.
#[derive(Debug, thiserror::Error)]
pub(crate) enum QmpError {
    #[error("failed to connect to QMP socket {socket}: {source}")]
    Connect {
        socket: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to set a QMP read timeout: {source}")]
    SetTimeout {
        #[source]
        source: std::io::Error,
    },
    #[error("failed to clone QMP socket: {source}")]
    CloneSocket {
        #[source]
        source: std::io::Error,
    },
    #[error("QMP socket opened with {greeting} instead of a greeting")]
    MissingGreeting { greeting: Value },
    #[error("failed to encode a QMP command: {source}")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to {step} the QMP command {command}: {source}")]
    Send {
        /// `send` or `flush`, the write step that failed.
        step: &'static str,
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("QMP command {command} failed: {error}")]
    Refused { command: String, error: Value },
    #[error("failed to read from the QMP socket: {source}")]
    Read {
        #[source]
        source: std::io::Error,
    },
    #[error("the QMP socket closed while waiting for a response")]
    Closed,
    #[error("failed to decode the QMP message {line:?}: {source}")]
    DecodeMessage {
        line: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to decode a query-balloon response: {source}")]
    DecodeBalloon {
        #[source]
        source: serde_json::Error,
    },
    #[error("the screendump path must be valid UTF-8, and {path:?} is not")]
    ScreendumpPathNotUtf8 { path: String },
}

/// Why a value the caller wrote is not one QEMU's input layer names.
///
/// Both are invariants of the value types below rather than checks the
/// call sites repeat: a [`QKeyCode`] that exists holds a key name shaped
/// the way QEMU spells its own, and an [`AbsCoordinate`] that exists is
/// inside the axis range QEMU reports positions on.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InputValueError {
    #[error(
        "{text:?} is not a QEMU key name: they are lowercase words like `a`, `ret`, `spc` or \
         `kp_enter`"
    )]
    NotAKeyName { text: String },
    #[error("the absolute coordinate {value} is outside QEMU's 0..={ABS_AXIS_MAX} axis range")]
    AbsOutOfRange { value: u32 },
}

/// The largest value QEMU's absolute pointer axes take.
///
/// QEMU normalises every absolute position onto one fixed range and the
/// guest's tablet reports that range as its own, so a script names a
/// position in it rather than in guest pixels — which the host cannot
/// know.
pub(crate) const ABS_AXIS_MAX: u32 = 0x7fff;

/// A QEMU key name, as `QKeyCode` spells it.
///
/// The set is QEMU's and moves with QEMU, so this type holds the shape
/// of a name rather than a list of them: a name that is shaped right
/// but unknown is refused by QEMU itself, naming the key it could not
/// find.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(crate) struct QKeyCode(String);

impl QKeyCode {
    pub(crate) fn new(text: &str) -> Result<Self, InputValueError> {
        let shaped = !text.is_empty()
            && text.chars().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
            });
        if !shaped {
            return Err(InputValueError::NotAKeyName {
                text: text.to_owned(),
            });
        }
        Ok(Self(text.to_owned()))
    }
}

/// A position on one of QEMU's absolute pointer axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(crate) struct AbsCoordinate(u32);

impl AbsCoordinate {
    pub(crate) fn new(value: u32) -> Result<Self, InputValueError> {
        if value > ABS_AXIS_MAX {
            return Err(InputValueError::AbsOutOfRange { value });
        }
        Ok(Self(value))
    }
}

/// Which axis a pointer event moves along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PointerAxis {
    X,
    Y,
}

/// A pointer button, as QEMU's input layer names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PointerButton {
    Left,
    Right,
    Middle,
}

/// Which key an [`InputEvent::Key`] names.
///
/// QEMU takes either its own key name or a raw scancode; the inspector
/// names keys, because a scancode means a different key on every
/// keyboard layout the guest might have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
pub(crate) enum KeyValue {
    QCode(QKeyCode),
}

/// One event of QEMU's `input-send-event`.
///
/// QEMU's `InputEvent` is a discriminated union — a `type` token and a
/// `data` object whose shape that token selects — which is what serde's
/// adjacent tagging renders, so the wire form is derived from these
/// variants instead of being written out by hand at the call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
pub(crate) enum InputEvent {
    Key {
        down: bool,
        key: KeyValue,
    },
    Btn {
        down: bool,
        button: PointerButton,
    },
    Abs {
        axis: PointerAxis,
        value: AbsCoordinate,
    },
    Rel {
        axis: PointerAxis,
        value: i32,
    },
}

/// The arguments of one `screendump`.
///
/// `format` is named on every capture: QEMU's default is PPM, and a
/// lane that uploaded a PPM under a `.png` name would produce an
/// artifact nothing opens.
#[derive(Debug, Serialize)]
struct ScreendumpArguments<'a> {
    filename: &'a str,
    format: &'a str,
}

/// The image format every capture asks for.
const SCREENDUMP_FORMAT: &str = "png";

/// The arguments of one `input-send-event`.
///
/// QEMU delivers the whole list to the guest as one input batch ended
/// by a sync, which is what makes a two-axis pointer move land as a
/// single position rather than as two.
#[derive(Debug, Serialize)]
struct InputSendEventArguments<'a> {
    events: &'a [InputEvent],
}

/// Why a size the caller wrote is not one QEMU would accept.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SizeError {
    #[error("{text:?} is not a size QEMU would accept: {source}")]
    NotANumber {
        text: String,
        #[source]
        source: core::num::ParseIntError,
    },
    #[error("the size {text:?} overflows a 64-bit byte count")]
    Overflow { text: String },
}

/// One QMP session over a QEMU monitor socket.
pub(crate) struct QmpClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

/// What `query-balloon` reports.
#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct BalloonInfo {
    /// The guest memory size QEMU currently allows, in bytes. The
    /// balloon holds the difference between this and `-m`.
    pub(crate) actual: u64,
}

impl QmpClient {
    /// Connects to `socket` and completes the capability handshake.
    pub(crate) fn connect(socket: &Path) -> Result<Self, QmpError> {
        let stream = UnixStream::connect(socket).map_err(|source| QmpError::Connect {
            socket: socket.display().to_string(),
            source,
        })?;
        stream
            .set_read_timeout(Some(RESPONSE_TIMEOUT))
            .map_err(|source| QmpError::SetTimeout { source })?;
        let writer = stream
            .try_clone()
            .map_err(|source| QmpError::CloneSocket { source })?;
        let mut client = Self {
            reader: BufReader::new(stream),
            writer,
        };
        // The greeting arrives unprompted and has to be consumed before
        // the handshake, or the handshake's own reply is read as the
        // greeting.
        let greeting = client.read_message()?;
        if greeting.get("QMP").is_none() {
            return Err(QmpError::MissingGreeting { greeting });
        }
        client.execute("qmp_capabilities", Value::Null)?;
        Ok(client)
    }

    /// Sets the balloon target: the guest memory size QEMU asks the
    /// guest to keep, in bytes.
    pub(crate) fn set_balloon(&mut self, bytes: u64) -> Result<(), QmpError> {
        self.execute("balloon", json!({ "value": bytes }))?;
        Ok(())
    }

    /// Writes the machine's current scanout to `path` as a PNG.
    ///
    /// The capture is QEMU's own view of the display device's surface,
    /// so it is taken with no host display backend at all: a headless
    /// lane sees exactly what a window would have shown.
    pub(crate) fn screendump(&mut self, path: &Path) -> Result<(), QmpError> {
        // QMP is JSON, and JSON strings are Unicode: a path that is not
        // UTF-8 cannot be named to QEMU at all, and lossy-encoding one
        // would write the capture somewhere the caller did not ask for.
        let filename = path
            .to_str()
            .ok_or_else(|| QmpError::ScreendumpPathNotUtf8 {
                path: path.display().to_string(),
            })?;
        let arguments = serde_json::to_value(ScreendumpArguments {
            filename,
            format: SCREENDUMP_FORMAT,
        })
        .map_err(|source| QmpError::Encode { source })?;
        self.execute("screendump", arguments)?;
        Ok(())
    }

    /// Delivers one batch of input events to the guest.
    pub(crate) fn input_send_event(&mut self, events: &[InputEvent]) -> Result<(), QmpError> {
        let arguments = serde_json::to_value(InputSendEventArguments { events })
            .map_err(|source| QmpError::Encode { source })?;
        self.execute("input-send-event", arguments)?;
        Ok(())
    }

    /// Reads what the guest has actually given up.
    pub(crate) fn query_balloon(&mut self) -> Result<BalloonInfo, QmpError> {
        let value = self.execute("query-balloon", Value::Null)?;
        serde_json::from_value(value).map_err(|source| QmpError::DecodeBalloon { source })
    }

    fn execute(&mut self, command: &str, arguments: Value) -> Result<Value, QmpError> {
        let mut request = json!({ "execute": command });
        if !arguments.is_null() {
            request["arguments"] = arguments;
        }
        let mut line =
            serde_json::to_vec(&request).map_err(|source| QmpError::Encode { source })?;
        line.push(b'\n');
        let write = |step| {
            move |source| QmpError::Send {
                step,
                command: command.to_owned(),
                source,
            }
        };
        self.writer.write_all(&line).map_err(write("send"))?;
        self.writer.flush().map_err(write("flush"))?;

        loop {
            let message = self.read_message()?;
            if let Some(error) = message.get("error") {
                return Err(QmpError::Refused {
                    command: command.to_owned(),
                    error: error.clone(),
                });
            }
            if let Some(result) = message.get("return") {
                return Ok(result.clone());
            }
            // Anything else is an asynchronous event, which is not this
            // command's answer.
        }
    }

    fn read_message(&mut self) -> Result<Value, QmpError> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = self
                .reader
                .read_line(&mut line)
                .map_err(|source| QmpError::Read { source })?;
            if read == 0 {
                return Err(QmpError::Closed);
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(&line).map_err(|source| QmpError::DecodeMessage {
                line: line.clone(),
                source,
            });
        }
    }
}

/// Parses a QEMU-style size: a decimal count with an optional
/// `K`/`M`/`G` suffix, as `-m` and the monitor's own `balloon` command
/// take.
pub(crate) fn parse_size(text: &str) -> Result<u64, SizeError> {
    let text = text.trim();
    let (digits, scale) = match text.chars().last() {
        Some('K' | 'k') => (&text[..text.len() - 1], 1024_u64),
        Some('M' | 'm') => (&text[..text.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    let value: u64 = digits.parse().map_err(|source| SizeError::NotANumber {
        text: text.to_owned(),
        source,
    })?;
    value.checked_mul(scale).ok_or_else(|| SizeError::Overflow {
        text: text.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        ABS_AXIS_MAX, AbsCoordinate, InputEvent, InputSendEventArguments, KeyValue, PointerAxis,
        PointerButton, QKeyCode, ScreendumpArguments, parse_size,
    };

    #[test]
    fn a_capture_names_the_format_it_wants() {
        let arguments = serde_json::to_value(ScreendumpArguments {
            filename: "/tmp/desktop.png",
            format: super::SCREENDUMP_FORMAT,
        })
        .expect("the screendump arguments serialise");
        assert_eq!(
            arguments,
            json!({ "filename": "/tmp/desktop.png", "format": "png" })
        );
    }

    #[test]
    fn input_events_render_qemus_own_union() {
        let events = [
            InputEvent::Key {
                down: true,
                key: KeyValue::QCode(QKeyCode::new("kp_enter").expect("a QEMU key name")),
            },
            InputEvent::Btn {
                down: false,
                button: PointerButton::Right,
            },
            InputEvent::Abs {
                axis: PointerAxis::X,
                value: AbsCoordinate::new(16_384).expect("inside the axis range"),
            },
            InputEvent::Rel {
                axis: PointerAxis::Y,
                value: -12,
            },
        ];
        let arguments =
            serde_json::to_value(InputSendEventArguments { events: &events }).expect("serialises");
        assert_eq!(
            arguments,
            json!({
                "events": [
                    { "type": "key", "data": { "down": true, "key": { "type": "qcode", "data": "kp_enter" } } },
                    { "type": "btn", "data": { "down": false, "button": "right" } },
                    { "type": "abs", "data": { "axis": "x", "value": 16384 } },
                    { "type": "rel", "data": { "axis": "y", "value": -12 } },
                ]
            })
        );
    }

    #[test]
    fn a_key_name_is_shaped_the_way_qemu_spells_them() {
        QKeyCode::new("a").expect("a single letter");
        QKeyCode::new("f12").expect("a function key");
        QKeyCode::new("shift_r").expect("an underscored name");
        QKeyCode::new("").expect_err("an empty name names no key");
        QKeyCode::new("Return").expect_err("QEMU's names are lowercase");
        QKeyCode::new("kp enter").expect_err("a name is one word");
    }

    #[test]
    fn an_absolute_coordinate_stays_inside_qemus_axis_range() {
        AbsCoordinate::new(0).expect("the origin");
        AbsCoordinate::new(ABS_AXIS_MAX).expect("the far edge");
        AbsCoordinate::new(ABS_AXIS_MAX + 1).expect_err("one past the far edge");
    }

    #[test]
    fn sizes_take_the_suffixes_qemu_takes() {
        assert_eq!(parse_size("1024").expect("bare byte count"), 1024);
        assert_eq!(parse_size("2K").expect("kibibytes"), 2 * 1024);
        assert_eq!(parse_size("1536M").expect("mebibytes"), 1536 * 1024 * 1024);
        assert_eq!(parse_size("2G").expect("gibibytes"), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size(" 4g ").expect("padded"), 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn a_size_that_is_not_a_number_is_rejected() {
        parse_size("plenty").expect_err("a bare word is not a size");
        parse_size("1T").expect_err("only K, M and G are QEMU's suffixes");
    }
}
