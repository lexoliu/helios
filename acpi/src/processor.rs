//! Processor objects as hotplug targets.
//!
//! Firmware announces a hot-added processor with `Notify (processor, 1)`
//! (device check; QEMU's CPU hotplug AML, `hw/acpi/cpu.c`) and asks for
//! one back with `Notify (processor, 3)` (eject request). The notified
//! object is a `Processor` (ACPI 1.0's `ProcessorOp`, which QEMU's x86
//! AML still emits) or a device whose `_HID` is `ACPI0007`. Which
//! processor of the possible set it is comes from its `_MAT`: the MADT
//! interrupt-controller structure the processor would have had at boot,
//! whose identity (a local APIC id, a GIC MPIDR) the backend maps to its
//! slot. `_EJ0` releases an ejected processor, and `_OST` reports the
//! kernel's answer (ACPI 6.5 §6.3.5).
//!
//! Concurrency contract: everything here evaluates AML, so it runs where
//! the interpreter's one thread of control is (`namespace`), never from
//! an interrupt.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use acpi::Handler;
use acpi::aml::AmlError;
use acpi::aml::namespace::AmlName;
use acpi::aml::object::{Object, ObjectType};
use helios_hal::cpu::ProcessorId;
use helios_hal::processor_hotplug::{HotplugAction, HotplugOutcome, HotplugRequest};

use crate::namespace::{Interpreter, child};
use crate::notify::Notification;

/// `Notify` values for device objects (ACPI 6.5 §5.6.6, Table 5.207).
const BUS_CHECK: u64 = 0x00;
const DEVICE_CHECK: u64 = 0x01;
const EJECT_REQUEST: u64 = 0x03;
/// The processor-specific values (ACPI 6.5 §8.4): a changed `_PPC`,
/// `_CST` or `_TPC`. Helios manages no performance, power or throttling
/// state, so they ask nothing of it.
const PROCESSOR_STATE_CHANGES: core::ops::RangeInclusive<u64> = 0x80..=0x82;

/// `_OST` status codes (ACPI 6.5 §6.3.5, Tables 6.13 and 6.14).
const OST_SUCCESS: u64 = 0x00;
const OST_NON_SPECIFIC_FAILURE: u64 = 0x01;
const OST_UNRECOGNIZED_NOTIFY: u64 = 0x02;
const OST_EJECT_NOT_SUPPORTED: u64 = 0x80;
const OST_DEVICE_BUSY: u64 = 0x82;

/// `_HID` of a processor device (ACPI 6.5 §8.4).
const PROCESSOR_DEVICE_HID: &str = "ACPI0007";

/// The `_EJ0` argument that ejects (ACPI 6.5 §6.3.3).
const EJECT: u64 = 1;

/// A processor's identity as its MADT interrupt-controller structure
/// gives it, decoded from the structures in its `_MAT`.
pub trait InterruptControllerEntry: Copy + fmt::Debug {
    /// Decodes `structure` (type byte, length byte and body), or `None`
    /// when it is not a processor structure of this architecture.
    fn decode(structure: &[u8]) -> Option<Self>;
}

/// An x86 processor's local APIC id, from a Processor Local APIC
/// structure (MADT type 0) or a Processor Local x2APIC structure (type 9;
/// ACPI 6.5 §5.2.12.2 and §5.2.12.12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalApicId(pub u32);

impl InterruptControllerEntry for LocalApicId {
    fn decode(structure: &[u8]) -> Option<Self> {
        const LOCAL_APIC: u8 = 0;
        const LOCAL_X2APIC: u8 = 9;
        match structure {
            // Type, length 8, processor UID, APIC id, flags.
            [LOCAL_APIC, 8, _uid, apic_id, ..] => Some(Self(u32::from(*apic_id))),
            // Type, length 16, two reserved bytes, x2APIC id, flags, UID.
            [LOCAL_X2APIC, 16, _, _, a, b, c, d, ..] => {
                Some(Self(u32::from_le_bytes([*a, *b, *c, *d])))
            }
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessorError {
    #[error("evaluating {path} failed: {error:?}")]
    Aml { path: AmlName, error: AmlError },
    #[error("{path} has no _MAT, which says which MADT processor it is")]
    MissingMat { path: AmlName },
    #[error("{path}._MAT is a {typ:?}, not a buffer of MADT structures")]
    MatNotBuffer { path: AmlName, typ: ObjectType },
    #[error("{path}._MAT {mat:02x?} holds no processor structure this architecture defines")]
    MatWithoutProcessor { path: AmlName, mat: Vec<u8> },
    #[error(
        "firmware notified {path}, whose _MAT names {entry}, but no processor of the possible \
         set the MADT described at boot has that identity"
    )]
    NoSlot { path: AmlName, entry: String },
    #[error(
        "processor {} has no namespace object: no request named it",
        processor.id()
    )]
    Unresolved { processor: ProcessorId },
}

/// The processor objects requests have named, and what they resolved to.
pub struct ProcessorObjects<Entry> {
    paths: BTreeMap<ProcessorId, AmlName>,
    entry: PhantomData<Entry>,
}

impl<Entry: InterruptControllerEntry> Default for ProcessorObjects<Entry> {
    fn default() -> Self {
        Self {
            paths: BTreeMap::new(),
            entry: PhantomData,
        }
    }
}

impl<Entry: InterruptControllerEntry> ProcessorObjects<Entry> {
    /// Turns the notifications an event method raised into hotplug
    /// requests, in order. `slot` maps a processor's `_MAT` identity to
    /// the possible set.
    ///
    /// A notification on anything but a processor, or a processor state
    /// change, asks nothing of the kernel and is dropped. A value the
    /// kernel does not recognise, or an eject request on a processor with
    /// no `_EJ0`, is answered through `_OST` here and raises no request.
    pub fn requests<H: Handler>(
        &mut self,
        interpreter: &Interpreter<H>,
        notifications: Vec<Notification>,
        mut slot: impl FnMut(Entry) -> Option<ProcessorId>,
    ) -> Result<Vec<HotplugRequest>, ProcessorError> {
        let mut requests = Vec::new();
        for Notification { object, value } in notifications {
            if !is_processor(interpreter, &object)? {
                tracing::debug!(%object, value, "Notify on a non-processor object ignored");
                continue;
            }
            let action = match value {
                BUS_CHECK | DEVICE_CHECK => HotplugAction::Insert,
                EJECT_REQUEST => HotplugAction::Eject,
                value if PROCESSOR_STATE_CHANGES.contains(&value) => {
                    tracing::debug!(%object, value, "processor state change ignored");
                    continue;
                }
                value => {
                    tracing::warn!(%object, value, "unrecognised Notify on a processor");
                    ost(interpreter, &object, value, OST_UNRECOGNIZED_NOTIFY)?;
                    continue;
                }
            };
            if action == HotplugAction::Eject && !has_object(interpreter, &object, "_EJ0")? {
                tracing::warn!(%object, "eject requested of a processor with no _EJ0");
                ost(interpreter, &object, value, OST_EJECT_NOT_SUPPORTED)?;
                continue;
            }
            let entry = processor_entry::<Entry, H>(interpreter, &object)?;
            let processor = slot(entry).ok_or_else(|| ProcessorError::NoSlot {
                path: object.clone(),
                entry: format!("{entry:?}"),
            })?;
            self.paths.insert(processor, object);
            requests.push(HotplugRequest { processor, action });
        }
        Ok(requests)
    }

    /// Runs `processor`'s `_EJ0`, which releases it to firmware.
    pub fn eject<H: Handler>(
        &self,
        interpreter: &Interpreter<H>,
        processor: ProcessorId,
    ) -> Result<(), ProcessorError> {
        let path = child_path(self.path(processor)?, "_EJ0")?;
        interpreter
            .evaluate(path.clone(), vec![Object::Integer(EJECT).wrap()])
            .map(drop)
            .map_err(|error| ProcessorError::Aml { path, error })
    }

    /// Reports `outcome` for `request` through the processor's `_OST`,
    /// when it has one.
    pub fn report<H: Handler>(
        &self,
        interpreter: &Interpreter<H>,
        request: HotplugRequest,
        outcome: HotplugOutcome,
    ) -> Result<(), ProcessorError> {
        let (source, status) = ost_answer(request.action, outcome);
        ost(interpreter, self.path(request.processor)?, source, status)
    }

    fn path(&self, processor: ProcessorId) -> Result<&AmlName, ProcessorError> {
        self.paths
            .get(&processor)
            .ok_or(ProcessorError::Unresolved { processor })
    }
}

/// The `_OST` source event and status code for an answered request.
fn ost_answer(action: HotplugAction, outcome: HotplugOutcome) -> (u64, u64) {
    match (action, outcome) {
        (HotplugAction::Insert, HotplugOutcome::Completed) => (DEVICE_CHECK, OST_SUCCESS),
        (HotplugAction::Insert, HotplugOutcome::Busy | HotplugOutcome::NotSupported) => {
            (DEVICE_CHECK, OST_NON_SPECIFIC_FAILURE)
        }
        (HotplugAction::Eject, HotplugOutcome::Completed) => (EJECT_REQUEST, OST_SUCCESS),
        (HotplugAction::Eject, HotplugOutcome::Busy) => (EJECT_REQUEST, OST_DEVICE_BUSY),
        (HotplugAction::Eject, HotplugOutcome::NotSupported) => {
            (EJECT_REQUEST, OST_EJECT_NOT_SUPPORTED)
        }
    }
}

fn child_path(object: &AmlName, segment: &str) -> Result<AmlName, ProcessorError> {
    child(object, segment).map_err(|error| ProcessorError::Aml {
        path: object.clone(),
        error,
    })
}

fn ost<H: Handler>(
    interpreter: &Interpreter<H>,
    object: &AmlName,
    source: u64,
    status: u64,
) -> Result<(), ProcessorError> {
    let path = child_path(object, "_OST")?;
    let arguments = vec![
        Object::Integer(source).wrap(),
        Object::Integer(status).wrap(),
        // No status information.
        Object::Buffer(Vec::new()).wrap(),
    ];
    interpreter
        .evaluate_if_present(path.clone(), arguments)
        .map(drop)
        .map_err(|error| ProcessorError::Aml { path, error })
}

fn has_object<H: Handler>(
    interpreter: &Interpreter<H>,
    object: &AmlName,
    segment: &str,
) -> Result<bool, ProcessorError> {
    let path = child_path(object, segment)?;
    match interpreter.namespace.lock().get(path.clone()) {
        Ok(_) => Ok(true),
        Err(AmlError::ObjectDoesNotExist(_) | AmlError::LevelDoesNotExist(_)) => Ok(false),
        Err(error) => Err(ProcessorError::Aml { path, error }),
    }
}

fn is_processor<H: Handler>(
    interpreter: &Interpreter<H>,
    object: &AmlName,
) -> Result<bool, ProcessorError> {
    let typ = interpreter
        .namespace
        .lock()
        .get(object.clone())
        .map_err(|error| ProcessorError::Aml {
            path: object.clone(),
            error,
        })?
        .typ();
    match typ {
        ObjectType::Processor => Ok(true),
        ObjectType::Device => {
            let path = child_path(object, "_HID")?;
            let hid = interpreter
                .evaluate_if_present(path.clone(), Vec::new())
                .map_err(|error| ProcessorError::Aml { path, error })?;
            Ok(matches!(hid.as_deref(), Some(Object::String(hid)) if hid == PROCESSOR_DEVICE_HID))
        }
        _ => Ok(false),
    }
}

/// The identity `object`'s `_MAT` gives.
fn processor_entry<Entry: InterruptControllerEntry, H: Handler>(
    interpreter: &Interpreter<H>,
    object: &AmlName,
) -> Result<Entry, ProcessorError> {
    let path = child_path(object, "_MAT")?;
    let mat = interpreter
        .evaluate_if_present(path.clone(), Vec::new())
        .map_err(|error| ProcessorError::Aml { path, error })?
        .ok_or_else(|| ProcessorError::MissingMat {
            path: object.clone(),
        })?;
    let Object::Buffer(bytes) = &*mat else {
        return Err(ProcessorError::MatNotBuffer {
            path: object.clone(),
            typ: mat.typ(),
        });
    };
    decode_mat(bytes).ok_or_else(|| ProcessorError::MatWithoutProcessor {
        path: object.clone(),
        mat: bytes.clone(),
    })
}

/// The first processor structure among the MADT structures in `mat`.
pub fn decode_mat<Entry: InterruptControllerEntry>(mat: &[u8]) -> Option<Entry> {
    let mut rest = mat;
    while let [_, length, ..] = rest {
        let length = usize::from(*length);
        let structure = rest.get(..length).filter(|_| length >= 2)?;
        if let Some(entry) = Entry::decode(structure) {
            return Some(entry);
        }
        rest = &rest[length..];
    }
    None
}
