//! The firmware's TPM event log: the record of every measurement the
//! chain before the kernel extended, one event per extend.
//!
//! The record layouts are the TCG PC Client Platform Firmware Profile's
//! (Version 1.05, sections 10.2.1 `TCG_PCClientPCREvent`, 10.2.2
//! `TCG_PCR_EVENT2` and 10.4.5.1 `TCG_EfiSpecIdEvent`), as the TCG EFI
//! Protocol Specification hands them out through
//! `EFI_TCG2_PROTOCOL.GetEventLog`. All fields are little-endian and
//! the records are packed. A TCG 1.2 log is a run of SHA-1 records; a
//! crypto-agile (TCG 2) log starts with one such record whose event is
//! the "Spec ID Event03" header naming every bank's digest size, and
//! every record after it carries one digest per bank.
//!
//! The log says what was measured, not that the registers hold it: a
//! register's value is the device's to report. The log is read here to
//! learn whether a loader measured a component at all.

use helios_hal::boot::MeasurementLogFormat;
use helios_hal::measured_boot::MeasurementRegister;
use thiserror::Error;

/// The size of a TCG 1.2 record's fixed part: register, event type, a
/// SHA-1 digest and the event size.
const TCG_PCR_EVENT_FIXED: usize = 4 + 4 + 20 + 4;
/// The signature the crypto-agile header's event starts with, NUL
/// included.
const SPEC_ID_EVENT03: &[u8; 16] = b"Spec ID Event03\0";
/// The header's fixed part before its digest sizes: signature, platform
/// class, the three version bytes, `uintnSize` and the algorithm count.
const SPEC_ID_FIXED: usize = 16 + 4 + 4 + 4;
/// How many banks a header may name. A TPM has a handful; Limine caps
/// its own walk at the same figure (`TCG_LOG_MAX_ALGORITHMS`).
const MAX_ALGORITHMS: usize = 16;

/// `EV_IPL`: an event a loader records for what it loads (TCG PC Client
/// Platform Firmware Profile, Table 9, `0x0000000D`).
pub const EV_IPL: u32 = 0x0000_000d;

/// Why the log cannot be read.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum EventLogError {
    #[error("the bootloader's event log format code {0} is neither TCG 1.2 (1) nor TCG 2 (2)")]
    UnknownFormat(u64),
    #[error("the record at byte {offset} runs past the end of the {len}-byte log")]
    Truncated { offset: usize, len: usize },
    #[error("the crypto-agile log does not start with a Spec ID Event03 header")]
    NoSpecIdHeader,
    #[error("the log's header names {count} banks; at most {MAX_ALGORITHMS} are read")]
    TooManyAlgorithms { count: u32 },
    #[error(
        "the record at byte {offset} carries a digest of algorithm {algorithm:#06x}, which the header does not name"
    )]
    UnknownAlgorithm { offset: usize, algorithm: u16 },
    #[error("the record at byte {offset} carries algorithm {algorithm:#06x} twice")]
    RepeatedAlgorithm { offset: usize, algorithm: u16 },
}

/// One measurement the log records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event<'a> {
    /// The register index the event's digests were extended into.
    pub register: u32,
    pub event_type: u32,
    /// The event's data: for an `EV_IPL`, the loader's description of
    /// what it measured.
    pub data: &'a [u8],
}

/// A measurement a loader is known to record: the register, the event
/// type and how the event's description starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoaderEvent {
    pub register: MeasurementRegister,
    pub event_type: u32,
    pub description_prefix: &'static [u8],
}

impl LoaderEvent {
    fn matches(&self, event: &Event<'_>) -> bool {
        event.register == u32::from(self.register.index())
            && event.event_type == self.event_type
            && event.data.starts_with(self.description_prefix)
    }
}

/// The event Limine records when it measures the kernel image: an
/// `EV_IPL` in PCR 9 whose description is `path: <kernel_path>`
/// (Limine 12.9.6 `USAGE.md`, "Measured Boot"; `common/protos/limine.c`,
/// `tpm_measure(TPM_PCR_LOADED_IMAGES, TPM_EV_IPL, kernel, …, "path: ",
/// kernel_path)`). PCR 8 carries an event with the same description over
/// the path string alone, which the register tells apart.
pub const LIMINE_KERNEL_IMAGE: LoaderEvent = LoaderEvent {
    register: MeasurementRegister::new(9),
    event_type: EV_IPL,
    description_prefix: b"path: ",
};

/// The banks a crypto-agile log's records carry, by algorithm and
/// digest size.
#[derive(Clone, Copy, Debug)]
struct DigestSizes {
    algorithms: [(u16, u16); MAX_ALGORITHMS],
    count: usize,
}

impl DigestSizes {
    fn size_of(&self, algorithm: u16) -> Option<(usize, u16)> {
        self.algorithms[..self.count]
            .iter()
            .enumerate()
            .find(|(_, (id, _))| *id == algorithm)
            .map(|(index, (_, size))| (index, *size))
    }
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    Tcg12,
    CryptoAgile(DigestSizes),
}

/// A firmware event log, its header checked.
#[derive(Clone, Copy, Debug)]
pub struct EventLog<'a> {
    bytes: &'a [u8],
    layout: Layout,
    /// Where the first measurement record starts.
    first: usize,
}

/// Reads little-endian fields out of the log, refusing any that runs
/// past its end.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
    record: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], EventLogError> {
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(EventLogError::Truncated {
                offset: self.record,
                len: self.bytes.len(),
            })?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, EventLogError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, EventLogError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("two bytes"),
        ))
    }

    fn u32(&mut self) -> Result<u32, EventLogError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }
}

/// Reads the TCG 1.2 record at `cursor`, the shape every record of a
/// TCG 1.2 log and the header of a crypto-agile one take.
fn tcg12_record<'a>(cursor: &mut Cursor<'a>) -> Result<Event<'a>, EventLogError> {
    let register = cursor.u32()?;
    let event_type = cursor.u32()?;
    cursor.take(TCG_PCR_EVENT_FIXED - 12)?;
    let size = cursor.u32()? as usize;
    Ok(Event {
        register,
        event_type,
        data: cursor.take(size)?,
    })
}

fn spec_id_header(data: &[u8]) -> Result<DigestSizes, EventLogError> {
    if data.len() < SPEC_ID_FIXED || &data[..16] != SPEC_ID_EVENT03 {
        return Err(EventLogError::NoSpecIdHeader);
    }
    let mut cursor = Cursor {
        bytes: data,
        at: SPEC_ID_FIXED - 4,
        record: 0,
    };
    let count = cursor.u32()?;
    if count as usize > MAX_ALGORITHMS {
        return Err(EventLogError::TooManyAlgorithms { count });
    }
    let mut sizes = DigestSizes {
        algorithms: [(0, 0); MAX_ALGORITHMS],
        count: 0,
    };
    for _ in 0..count {
        let algorithm = cursor.u16()?;
        let size = cursor.u16()?;
        if sizes.size_of(algorithm).is_some() {
            return Err(EventLogError::RepeatedAlgorithm {
                offset: 0,
                algorithm,
            });
        }
        sizes.algorithms[sizes.count] = (algorithm, size);
        sizes.count += 1;
    }
    let vendor = cursor.u8()? as usize;
    cursor.take(vendor)?;
    Ok(sizes)
}

impl<'a> EventLog<'a> {
    /// The log `bytes` holds, laid out as `format`.
    pub fn parse(format: MeasurementLogFormat, bytes: &'a [u8]) -> Result<Self, EventLogError> {
        match format {
            MeasurementLogFormat::Tcg12 => Ok(Self {
                bytes,
                layout: Layout::Tcg12,
                first: 0,
            }),
            MeasurementLogFormat::Tcg2 => {
                let mut cursor = Cursor {
                    bytes,
                    at: 0,
                    record: 0,
                };
                let header = tcg12_record(&mut cursor)?;
                Ok(Self {
                    bytes,
                    layout: Layout::CryptoAgile(spec_id_header(header.data)?),
                    first: cursor.at,
                })
            }
            MeasurementLogFormat::Other(code) => Err(EventLogError::UnknownFormat(code)),
        }
    }

    /// Every measurement record, in the order the log holds them; the
    /// crypto-agile header is not one.
    pub fn events(&self) -> impl Iterator<Item = Result<Event<'a>, EventLogError>> + '_ {
        let mut at = self.first;
        let mut failed = false;
        core::iter::from_fn(move || {
            if failed || at == self.bytes.len() {
                return None;
            }
            let mut cursor = Cursor {
                bytes: self.bytes,
                at,
                record: at,
            };
            let event = match &self.layout {
                Layout::Tcg12 => tcg12_record(&mut cursor),
                Layout::CryptoAgile(sizes) => crypto_agile_record(&mut cursor, sizes),
            };
            match event {
                Ok(event) => {
                    at = cursor.at;
                    Some(Ok(event))
                }
                Err(error) => {
                    failed = true;
                    Some(Err(error))
                }
            }
        })
    }

    /// Whether the log records `event`.
    pub fn records(&self, event: &LoaderEvent) -> Result<bool, EventLogError> {
        for recorded in self.events() {
            if event.matches(&recorded?) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Reads one `TCG_PCR_EVENT2`. A record may carry fewer digests than
/// the header names banks, as Limine lets through for the records it
/// appends, but never a bank the header does not name or one twice.
fn crypto_agile_record<'a>(
    cursor: &mut Cursor<'a>,
    sizes: &DigestSizes,
) -> Result<Event<'a>, EventLogError> {
    let offset = cursor.record;
    let register = cursor.u32()?;
    let event_type = cursor.u32()?;
    let count = cursor.u32()?;
    if count as usize > sizes.count {
        return Err(EventLogError::TooManyAlgorithms { count });
    }
    let mut seen = 0_u32;
    for _ in 0..count {
        let algorithm = cursor.u16()?;
        let (index, size) = sizes
            .size_of(algorithm)
            .ok_or(EventLogError::UnknownAlgorithm { offset, algorithm })?;
        if seen & (1 << index) != 0 {
            return Err(EventLogError::RepeatedAlgorithm { offset, algorithm });
        }
        seen |= 1 << index;
        cursor.take(usize::from(size))?;
    }
    let size = cursor.u32()? as usize;
    Ok(Event {
        register,
        event_type,
        data: cursor.take(size)?,
    })
}
