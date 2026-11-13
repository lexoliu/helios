//! The general-purpose event blocks the FADT describes, and the AML
//! methods that serve them.
//!
//! A GPE block is two equal halves of byte registers (ACPI 6.5 §4.8.4.1):
//! the status bytes, then the enable bytes; byte `n` of a half covers GPEs
//! `base + 8n ..= base + 8n + 7`, and a status bit is cleared by writing it
//! as one. A GPE that is both pending and enabled asserts the SCI. Its
//! method is `\_GPE._Exx` for an edge-triggered event and `\_GPE._Lxx`
//! for a level-triggered one (ACPI 6.5 §5.6.4), `xx` the GPE number in
//! two uppercase hex digits.
//!
//! # Concurrency contract
//!
//! [`GpeRegisters::latch`] runs in the SCI handler; every other operation
//! runs on a task on the processor the SCI is routed to, and only there.
//! The latch masks the GPEs it found pending before it returns, which is
//! what deasserts a level-triggered SCI; the task re-enables each one once
//! its method has run. Both sides read-modify-write the enable bytes, so
//! the task side masks that processor's interrupts around each write
//! ([`helios_hal::critical_section::with_local_interrupts_masked`]), and
//! the latched set the two exchange is atomic.

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::fmt;
use core::str::FromStr;
use core::sync::atomic::{AtomicU8, Ordering};

use acpi::Handler;
use acpi::aml::AmlError;
use acpi::aml::namespace::AmlName;
use acpi::aml::object::ObjectType;
use acpi::sdt::fadt::Fadt;
use helios_hal::critical_section::with_local_interrupts_masked;

use crate::namespace::Interpreter;

/// A general-purpose event number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Gpe(pub u16);

impl fmt::Display for Gpe {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "GPE {:#04x}", self.0)
    }
}

/// How a GPE's method is triggered, which fixes when its status is
/// cleared: before the method for an edge, after it for a level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpeTrigger {
    Edge,
    Level,
}

#[derive(Debug, thiserror::Error)]
pub enum GpeError {
    #[error(
        "GPE{block} block is in address space {space:#x}; general-purpose event registers \
         are reached through system I/O"
    )]
    AddressSpace { block: u8, space: u8 },
    #[error("GPE{block} block at port {port:#x} with {length} bytes runs past the I/O space")]
    PortRange { block: u8, port: u64, length: u8 },
    #[error("GPE{block} block has odd length {length}; it is two equal halves")]
    OddLength { block: u8, length: u8 },
    #[error("{gpe} is served by both {path}._E and ._L methods; a GPE has one trigger")]
    BothTriggers { gpe: Gpe, path: AmlName },
    #[error("{path} is a {typ:?}, where ACPI names a GPE's method")]
    NotAMethod { path: AmlName, typ: ObjectType },
    #[error("looking up {path} failed: {error:?}")]
    Lookup { path: AmlName, error: AmlError },
    #[error("{gpe} was latched, but no method serves it; only GPEs with a method are enabled")]
    NoMethod { gpe: Gpe },
    #[error("evaluating {path} for {gpe} failed: {error:?}")]
    Method {
        gpe: Gpe,
        path: AmlName,
        error: AmlError,
    },
}

/// One block's place in I/O space.
#[derive(Clone, Copy, Debug)]
struct GpeBlock {
    /// The GPE number of bit 0 of the first status byte.
    first: u16,
    /// The first status byte's port; the enable bytes follow the status
    /// bytes.
    port: u16,
    /// Bytes in each half.
    half: u16,
}

/// The FADT's GPE blocks, and the GPEs the SCI handler has latched.
pub struct GpeRegisters<H> {
    handler: H,
    blocks: Vec<GpeBlock>,
    /// One byte per status register across the blocks, in block order:
    /// the bits the SCI handler found pending and enabled, and masked.
    latched: Box<[AtomicU8]>,
}

impl<H: Handler> GpeRegisters<H> {
    /// The GPE0 and GPE1 blocks `fadt` describes, reached through
    /// `handler`'s port I/O. A machine with neither has no GPEs.
    pub fn from_fadt(fadt: &Fadt, handler: H) -> Result<Self, GpeError> {
        let revision = fadt.header.revision;
        // SAFETY: the extended fields exist from FADT revision 2, which
        // `access` checks against the table's own revision.
        let x_gpe0 = unsafe { { fadt.x_gpe0_block }.access(revision) };
        // SAFETY: as above.
        let x_gpe1 = unsafe { { fadt.x_gpe1_block }.access(revision) };
        let described = [
            (0, x_gpe0, fadt.gpe0_block, fadt.gpe0_block_length, 0),
            (
                1,
                x_gpe1,
                fadt.gpe1_block,
                fadt.gpe1_block_length,
                u16::from(fadt.gpe1_base),
            ),
        ];
        let mut blocks = Vec::new();
        for (block, extended, legacy, length, first) in described {
            // The extended address wins when it is set; the length is the
            // FADT's either way (ACPI 6.5 §5.2.9, `X_GPE0_BLK`).
            let (space, address) = match extended {
                Some(gas) if { gas.address } != 0 => (gas.address_space, { gas.address }),
                _ => (SYSTEM_IO, u64::from(legacy)),
            };
            if address == 0 || length == 0 {
                continue;
            }
            if space != SYSTEM_IO {
                return Err(GpeError::AddressSpace { block, space });
            }
            if length % 2 != 0 {
                return Err(GpeError::OddLength { block, length });
            }
            let port = u16::try_from(address)
                .ok()
                .filter(|port| port.checked_add(u16::from(length) - 1).is_some())
                .ok_or(GpeError::PortRange {
                    block,
                    port: address,
                    length,
                })?;
            blocks.push(GpeBlock {
                first,
                port,
                half: u16::from(length / 2),
            });
        }
        let registers: usize = blocks.iter().map(|block| usize::from(block.half)).sum();
        Ok(Self {
            handler,
            blocks,
            latched: (0..registers).map(|_| AtomicU8::new(0)).collect(),
        })
    }

    /// Every status register: its index into `latched`, its port, and
    /// its enable register's port and first GPE.
    fn registers(&self) -> impl Iterator<Item = Register> + '_ {
        self.blocks
            .iter()
            .flat_map(|block| {
                (0..block.half).map(move |byte| Register {
                    index: 0,
                    status: block.port + byte,
                    enable: block.port + block.half + byte,
                    first: block.first + 8 * byte,
                })
            })
            .enumerate()
            .map(|(index, register)| Register { index, ..register })
    }

    /// Every GPE the blocks implement.
    pub fn gpes(&self) -> impl Iterator<Item = Gpe> + '_ {
        self.registers()
            .flat_map(|register| (0..8).map(move |bit| Gpe(register.first + bit)))
    }

    /// Disables every GPE and clears every status, so that nothing
    /// asserts the SCI until a GPE is enabled on purpose.
    pub fn disable_all(&self) {
        for register in self.registers() {
            with_local_interrupts_masked(|| self.handler.write_io_u8(register.enable, 0));
            self.handler.write_io_u8(register.status, 0xff);
        }
    }

    /// Enables `gpe`, which must be one the blocks implement.
    pub fn enable(&self, gpe: Gpe) {
        let (register, bit) = self.locate(gpe);
        with_local_interrupts_masked(|| {
            let enabled = self.handler.read_io_u8(register.enable);
            self.handler.write_io_u8(register.enable, enabled | bit);
        });
    }

    /// Clears `gpe`'s status.
    pub fn clear_status(&self, gpe: Gpe) {
        let (register, bit) = self.locate(gpe);
        // Write-one-to-clear: the other bits are written as zero and stay.
        self.handler.write_io_u8(register.status, bit);
    }

    /// The SCI handler's half: masks every GPE that is pending and
    /// enabled, remembers it, and answers whether there was one.
    pub fn latch(&self) -> bool {
        let mut any = false;
        for register in self.registers() {
            let enabled = self.handler.read_io_u8(register.enable);
            if enabled == 0 {
                continue;
            }
            let active = self.handler.read_io_u8(register.status) & enabled;
            if active == 0 {
                continue;
            }
            self.handler.write_io_u8(register.enable, enabled & !active);
            self.latched[register.index].fetch_or(active, Ordering::AcqRel);
            any = true;
        }
        any
    }

    /// The GPEs latched since the previous call, lowest first. Each one
    /// stays masked until [`Self::enable`] re-enables it.
    pub fn take_latched(&self) -> Vec<Gpe> {
        self.registers()
            .flat_map(|register| {
                let bits = self.latched[register.index].swap(0, Ordering::AcqRel);
                (0..8)
                    .filter(move |bit| bits & (1 << bit) != 0)
                    .map(move |bit| Gpe(register.first + bit))
            })
            .collect()
    }

    fn locate(&self, gpe: Gpe) -> (Register, u8) {
        self.registers()
            .find(|register| (register.first..register.first + 8).contains(&gpe.0))
            .map(|register| (register, 1 << (gpe.0 - register.first)))
            .unwrap_or_else(|| panic!("{gpe} is not implemented by the FADT's GPE blocks"))
    }
}

/// The GAS address-space id of system I/O (ACPI 6.5 Table 5.25).
const SYSTEM_IO: u8 = 1;

#[derive(Clone, Copy)]
struct Register {
    index: usize,
    status: u16,
    enable: u16,
    first: u16,
}

/// One GPE's method.
#[derive(Clone, Debug)]
pub struct GpeMethod {
    pub gpe: Gpe,
    pub trigger: GpeTrigger,
    pub path: AmlName,
}

/// The methods the namespace provides for the blocks' GPEs: the GPEs the
/// kernel can serve, and so the ones it enables.
pub struct GpeMethods {
    methods: Vec<GpeMethod>,
}

impl GpeMethods {
    /// Finds `\_GPE._Exx` and `\_GPE._Lxx` for every GPE the blocks
    /// implement. A GPE above 0xff has no method name and is never
    /// served.
    pub fn discover<A: Handler, H: Handler>(
        interpreter: &Interpreter<A>,
        registers: &GpeRegisters<H>,
    ) -> Result<Self, GpeError> {
        let mut methods = Vec::new();
        for gpe in registers.gpes() {
            let Ok(number) = u8::try_from(gpe.0) else {
                continue;
            };
            let edge = method_path('E', number);
            let level = method_path('L', number);
            let found = (
                method_exists(interpreter, &edge)?,
                method_exists(interpreter, &level)?,
            );
            let (trigger, path) = match found {
                (false, false) => continue,
                (true, false) => (GpeTrigger::Edge, edge),
                (false, true) => (GpeTrigger::Level, level),
                (true, true) => return Err(GpeError::BothTriggers { gpe, path: edge }),
            };
            methods.push(GpeMethod { gpe, trigger, path });
        }
        Ok(Self { methods })
    }

    pub fn methods(&self) -> &[GpeMethod] {
        &self.methods
    }

    /// Enables every GPE that has a method.
    pub fn enable_all<H: Handler>(&self, registers: &GpeRegisters<H>) {
        for method in &self.methods {
            registers.enable(method.gpe);
        }
    }

    /// Runs `gpe`'s method, clearing its status on the side its trigger
    /// needs, and re-enables it.
    pub fn run<A: Handler, H: Handler>(
        &self,
        gpe: Gpe,
        interpreter: &Interpreter<A>,
        registers: &GpeRegisters<H>,
    ) -> Result<(), GpeError> {
        let method = self
            .methods
            .iter()
            .find(|method| method.gpe == gpe)
            .ok_or(GpeError::NoMethod { gpe })?;
        if method.trigger == GpeTrigger::Edge {
            // An edge that lands while the method runs sets the status
            // again and is served on the next pass.
            registers.clear_status(gpe);
        }
        interpreter
            .evaluate(method.path.clone(), Vec::new())
            .map_err(|error| GpeError::Method {
                gpe,
                path: method.path.clone(),
                error,
            })?;
        if method.trigger == GpeTrigger::Level {
            // The method has quieted the source, so the level is gone.
            registers.clear_status(gpe);
        }
        registers.enable(gpe);
        Ok(())
    }
}

fn method_path(trigger: char, gpe: u8) -> AmlName {
    let text = format!("\\_GPE._{trigger}{gpe:02X}");
    AmlName::from_str(&text)
        .unwrap_or_else(|error| panic!("{text} is not a valid AML name: {error:?}"))
}

fn method_exists<H: Handler>(
    interpreter: &Interpreter<H>,
    path: &AmlName,
) -> Result<bool, GpeError> {
    match interpreter.namespace.lock().get(path.clone()) {
        Ok(object) if object.typ() == ObjectType::Method => Ok(true),
        Ok(object) => Err(GpeError::NotAMethod {
            path: path.clone(),
            typ: object.typ(),
        }),
        Err(AmlError::ObjectDoesNotExist(_) | AmlError::LevelDoesNotExist(_)) => Ok(false),
        Err(error) => Err(GpeError::Lookup {
            path: path.clone(),
            error,
        }),
    }
}
