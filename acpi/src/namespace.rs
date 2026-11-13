//! One AML namespace built from the DSDT and every SSDT.
//!
//! Concurrency contract: an [`Interpreter`] has one thread of control.
//! Its handler's mutexes are the backend's, and the backends this tree
//! has evaluate methods from one processor only; nothing here adds
//! exclusion of its own.

use core::str::FromStr;

use acpi::aml::namespace::AmlName;
use acpi::aml::op_region::{OpRegion, RegionHandler};
use acpi::aml::{AmlError, BaseInterpreter};
use acpi::registers::FixedRegisters;
use acpi::sdt::SdtHeader;
use acpi::sdt::fadt::Fadt;
use acpi::{AcpiError, AcpiTables, AmlTable, Handler, PhysicalMapping};
use alloc::sync::Arc;

/// The region handler type of the interpreters this tree builds.
///
/// No backend installs an operation-region handler: the interpreter
/// serves system-memory, system-I/O and PCI-configuration regions itself,
/// through its [`Handler`]. The type has no values, which keeps the
/// interpreter free of the
/// `dyn RegionHandler` the `acpi` crate's own `Interpreter` alias imposes.
pub enum NoRegionHandler {}

impl RegionHandler for NoRegionHandler {
    fn read_u8(&self, _region: &OpRegion) -> Result<u8, AmlError> {
        match *self {}
    }

    fn read_u16(&self, _region: &OpRegion) -> Result<u16, AmlError> {
        match *self {}
    }

    fn read_u32(&self, _region: &OpRegion) -> Result<u32, AmlError> {
        match *self {}
    }

    fn read_u64(&self, _region: &OpRegion) -> Result<u64, AmlError> {
        match *self {}
    }

    fn write_u8(&self, _region: &OpRegion, _value: u8) -> Result<(), AmlError> {
        match *self {}
    }

    fn write_u16(&self, _region: &OpRegion, _value: u16) -> Result<(), AmlError> {
        match *self {}
    }

    fn write_u32(&self, _region: &OpRegion, _value: u32) -> Result<(), AmlError> {
        match *self {}
    }

    fn write_u64(&self, _region: &OpRegion, _value: u64) -> Result<(), AmlError> {
        match *self {}
    }
}

/// The AML interpreter, over the handler `H`.
pub type Interpreter<H> = BaseInterpreter<H, NoRegionHandler>;

/// Why the namespace could not be built.
#[derive(Debug, thiserror::Error)]
pub enum NamespaceError {
    #[error("the ACPI tables carry no FADT")]
    MissingFadt,
    #[error("the FADT's fixed register blocks cannot be used: {0:?}")]
    FixedRegisters(AcpiError),
    #[error("the ACPI tables carry no DSDT: {0:?}")]
    MissingDsdt(AcpiError),
    #[error("{table_bytes}-byte AML table is shorter than its {header_bytes}-byte header")]
    Truncated {
        table_bytes: usize,
        header_bytes: usize,
    },
    #[error("loading the AML table at {address:#x} failed: {error:?}")]
    Load { address: usize, error: AmlError },
}

/// An empty namespace whose fixed registers are the FADT's, for tables of
/// the DSDT's `revision` (which fixes the integer width).
pub fn interpreter<H: Handler>(
    fadt: &Fadt,
    revision: u8,
    handler: H,
) -> Result<Interpreter<H>, NamespaceError> {
    // The constructor takes its register block as an `Arc`.
    let registers = Arc::new(
        FixedRegisters::new(fadt, handler.clone()).map_err(NamespaceError::FixedRegisters)?,
    );
    Ok(Interpreter::new(handler, revision, registers, None))
}

/// Loads one definition block, `table` being the whole table, header
/// included; `address` names it in an error.
pub fn load_definition_block<H: Handler>(
    interpreter: &Interpreter<H>,
    table: &[u8],
    address: usize,
) -> Result<(), NamespaceError> {
    let header_bytes = core::mem::size_of::<SdtHeader>();
    let stream = table.get(header_bytes..).ok_or(NamespaceError::Truncated {
        table_bytes: table.len(),
        header_bytes,
    })?;
    interpreter
        .load_table(stream)
        .map_err(|error| NamespaceError::Load { address, error })
}

/// Loads the DSDT and every SSDT `tables` lists into a fresh namespace
/// interpreted through `handler`.
pub fn load_namespace<T: Handler, H: Handler>(
    tables: &AcpiTables<T>,
    handler: &H,
) -> Result<Interpreter<H>, NamespaceError> {
    let fadt = tables
        .find_table::<Fadt>()
        .ok_or(NamespaceError::MissingFadt)?;
    let dsdt = tables.dsdt().map_err(NamespaceError::MissingDsdt)?;
    let interpreter = interpreter(&fadt, dsdt.revision, handler.clone())?;
    load_table(&interpreter, handler, dsdt)?;
    for ssdt in tables.ssdts() {
        load_table(&interpreter, handler, ssdt)?;
    }
    Ok(interpreter)
}

fn load_table<H: Handler>(
    interpreter: &Interpreter<H>,
    handler: &H,
    table: AmlTable,
) -> Result<(), NamespaceError> {
    let length = table.length as usize;
    // SAFETY: the address and length come from a table header the `acpi`
    // crate already validated, and the handler maps physical memory.
    let mapping =
        unsafe { PhysicalMapping::<_, u8>::new(table.phys_address, length, handler.clone()) };
    // SAFETY: the mapping covers `length` bytes and outlives the slice.
    let bytes = unsafe { core::slice::from_raw_parts(mapping.as_ptr(), length) };
    load_definition_block(interpreter, bytes, table.phys_address)
}

/// `scope.segment`, for a single name segment.
pub fn child(scope: &AmlName, segment: &str) -> Result<AmlName, AmlError> {
    AmlName::from_str(segment)?.resolve(scope)
}
