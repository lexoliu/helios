//! What every vCPU of a guest that stopped answering was doing.
//!
//! A guest that wedges without panicking leaves nothing behind: its
//! serial log ends at the boot, and the RPC that timed out says only
//! that nothing came back. Before QEMU is torn down the inspector asks
//! QEMU itself — which still holds every vCPU's architectural state —
//! for the register file and the local APIC of each vCPU, and for 4 KiB
//! of its current stack and of its page-fault IST stack (`stacks`). It
//! keeps the raw dumps in `vcpu-state.log`, and names in the run log the
//! function each vCPU was executing, whether it could take an interrupt,
//! and the candidate return addresses on each stack.
//!
//! The capture is a diagnostic of a failure that has already happened,
//! so it never replaces that failure: whatever it cannot do is reported
//! beside the timeout, and the timeout is what the run returns.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use console::strip_ansi_codes;

use self::stacks::{
    MemoryDump, STACK_DUMP_WORDS, StackDumpError, StackKind, StackReport, StackScan,
    TSS_IST1_OFFSET, dump_command, ist1_slot, page_fault_stack_start, parse_memory_dump,
};
use super::kernel_image::{KernelSymbols, KernelSymbolsError, SymbolizedAddress};
use super::qmp::{QmpClient, QmpError};
use super::{VmArch, arch_label};
use crate::workload_bench::WorkloadBenchError;

mod stacks;

/// The file each capture is appended to, in the runtime directory: the
/// bench workflow's `bench-runtime/**/*.log` upload keeps it.
pub(crate) const VCPU_STATE_LOG_NAME: &str = "vcpu-state.log";

/// The message of the x86 kernel's boot line that records where the
/// bootloader placed the image (`x86/src/lib.rs`).
const KERNEL_IMAGE_LOADED: &str = "kernel image loaded";

/// The field of that line carrying the runtime virtual base.
const VIRTUAL_BASE_FIELD: &str = "virtual_base";

/// The register dump of every vCPU.
const INFO_REGISTERS: &str = "info registers -a";

/// One vCPU's local APIC, chosen by QMP's `cpu-index`: interrupts
/// pending (IRR) and in service (ISR), the timer, and the task priority.
const INFO_LAPIC: &str = "info lapic";

/// The name the parser gives the base field of the `TR` line.
const TR_BASE: &str = "TR base";

/// RFLAGS.IF: the processor takes maskable interrupts when it is set.
const RFLAGS_INTERRUPT_ENABLE: u64 = 1 << 9;

/// Why a session cannot capture vCPU state at all.
///
/// Settled when the session starts, and reported at the timeout it
/// would have explained rather than at boot: a session that never times
/// out has nothing to say about it.
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum CaptureUnavailable {
    #[error(
        "the session has no inspector-owned QMP socket (a `--qmp` endpoint that is not \
         `unix:<path>` cannot be spoken to)"
    )]
    NoQmpSocket,
    #[error("the capture reads x86-64 registers and APICs, and this guest is {arch}")]
    Architecture { arch: &'static str },
}

/// Why a capture that was attempted produced no register dump.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CaptureError {
    #[error("{source}")]
    Connect {
        #[source]
        source: QmpError,
    },
    #[error("QEMU did not answer `{command}`: {source}")]
    Monitor {
        command: &'static str,
        #[source]
        source: QmpError,
    },
    #[error("`{INFO_REGISTERS}` output could not be read: {source}")]
    Registers {
        #[source]
        source: RegisterDumpError,
    },
    #[error("failed to write {path}: {source}")]
    WriteLog {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// Why the text of `info registers -a` is not the dump this parser
/// knows.
///
/// The format is QEMU's `x86_cpu_dump_state`: a `CPU#<n>` header per
/// vCPU, then `RIP=<hex> RFL=<hex> … HLT=<0|1>` (or `EIP=`/`EFL=` for a
/// vCPU outside long mode).
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RegisterDumpError {
    #[error("no `CPU#<n>` section in the monitor's answer {output:?}")]
    NoVcpus { output: String },
    #[error("`{header}` does not name a vCPU index")]
    BadHeader { header: String },
    #[error("register {register} appears twice for vCPU {vcpu}")]
    Duplicate { vcpu: u32, register: &'static str },
    #[error("vCPU {vcpu} has no {register} in the dump")]
    Missing { vcpu: u32, register: &'static str },
    #[error("vCPU {vcpu} register {register} reads {text:?}, which is not hexadecimal")]
    NotHex {
        vcpu: u32,
        register: &'static str,
        text: String,
    },
    #[error("vCPU {vcpu} HLT reads {text:?}, which is neither 0 nor 1")]
    NotAFlag { vcpu: u32, text: String },
}

/// Why a per-vCPU monitor command produced no answer.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MonitorError {
    #[error("QEMU did not answer `{command}` on vCPU {vcpu}: {source}")]
    Unanswered {
        vcpu: u32,
        command: String,
        #[source]
        source: QmpError,
    },
    #[error("not asked: QEMU had already stopped answering, at `{command}` on vCPU {vcpu}")]
    Abandoned { vcpu: u32, command: String },
}

/// Why the vCPUs' instruction pointers could not be named.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SymbolizeError {
    #[error("the session recorded no debug serial log to read the kernel's load base from")]
    NoSerialLog,
    #[error("failed to read the debug serial log {path}: {source}")]
    ReadSerialLog {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("the debug serial log {path} has no `{KERNEL_IMAGE_LOADED}` line")]
    NoBaseLine { path: String },
    #[error(
        "the `{KERNEL_IMAGE_LOADED}` line {line:?} carries no hexadecimal {VIRTUAL_BASE_FIELD}"
    )]
    MalformedBaseLine { line: String },
    #[error("{source}")]
    Kernel {
        #[from]
        source: KernelSymbolsError,
    },
}

/// The vCPU state capture a session runs when a guest step times out.
#[derive(Debug, Clone)]
pub(crate) struct VcpuStateCapture {
    probe: Result<VcpuStateProbe, CaptureUnavailable>,
}

/// Everything one capture reads from and writes to.
#[derive(Debug, Clone)]
struct VcpuStateProbe {
    qmp_socket: PathBuf,
    log: PathBuf,
    debug_serial_log: Option<PathBuf>,
    kernel: PathBuf,
}

impl VcpuStateCapture {
    /// The capture for a session booting `kernel` on `arch`.
    ///
    /// `debug_serial_log` is the raw copy of the guest's debug serial
    /// line, which carries the kernel's load base; a session without one
    /// still captures registers, only without symbols.
    pub(crate) fn new(
        arch: VmArch,
        qmp_socket: Option<PathBuf>,
        runtime_dir: &Path,
        debug_serial_log: Option<PathBuf>,
        kernel: PathBuf,
    ) -> Self {
        let probe = match (arch, qmp_socket) {
            (VmArch::X86_64, Some(qmp_socket)) => Ok(VcpuStateProbe {
                qmp_socket,
                log: runtime_dir.join(VCPU_STATE_LOG_NAME),
                debug_serial_log,
                kernel,
            }),
            (VmArch::X86_64, None) => Err(CaptureUnavailable::NoQmpSocket),
            (VmArch::Aarch64 | VmArch::Riscv64, _) => Err(CaptureUnavailable::Architecture {
                arch: arch_label(arch),
            }),
        };
        Self { probe }
    }

    /// Captures every vCPU's state after `timeout`, and reports it in
    /// the run log.
    ///
    /// Infallible by design: the caller returns `timeout` whatever
    /// happens here, and a capture that could not be taken says why in
    /// the run log instead of in place of it.
    pub(crate) async fn record(&self, timeout: &WorkloadBenchError) {
        let probe = match &self.probe {
            Ok(probe) => probe.clone(),
            Err(unavailable) => {
                eprintln!("helios-inspector: vCPU state not captured: {unavailable}");
                return;
            }
        };
        let trigger = timeout.to_string();
        // QMP is a blocking socket and the kernel image is tens of
        // megabytes; neither belongs on the executor thread.
        let captured = blocking::unblock(move || probe.capture(&trigger)).await;
        match captured {
            Ok(report) => report.print(),
            Err(error) => eprintln!("helios-inspector: vCPU state not captured: {error}"),
        }
    }
}

impl VcpuStateProbe {
    fn capture(&self, trigger: &str) -> Result<CaptureReport, CaptureError> {
        let mut qmp = QmpClient::connect(&self.qmp_socket)
            .map_err(|source| CaptureError::Connect { source })?;
        let mut log = CaptureLog::open(&self.log)?;
        log.section(&format!("vCPU state after: {trigger}"), "")?;

        let registers = qmp
            .human_monitor_command(INFO_REGISTERS)
            .map_err(|source| CaptureError::Monitor {
                command: INFO_REGISTERS,
                source,
            })?;
        log.section(INFO_REGISTERS, &registers)?;
        let vcpus =
            parse_registers(&registers).map_err(|source| CaptureError::Registers { source })?;

        // An APIC that cannot be read does not cost the registers already
        // in hand. It does end the APIC pass, and every command after it
        // (`Monitor`).
        let mut monitor = Monitor {
            qmp,
            silent_since: None,
        };
        let mut lapic_failure = None;
        for (position, vcpu) in vcpus.iter().enumerate() {
            let title = format!("{INFO_LAPIC} (vCPU {})", vcpu.index);
            match monitor.on(vcpu.index, INFO_LAPIC) {
                Ok(lapic) => log.section(&title, &lapic)?,
                Err(error) => {
                    log.section(&title, &format!("not captured: {error}\n"))?;
                    lapic_failure = Some(LapicFailure {
                        vcpu: vcpu.index,
                        not_asked: vcpus.len() - position - 1,
                        error,
                    });
                    break;
                }
            }
        }

        // The stacks are read through the same monitor conversation, so
        // a command QEMU left unanswered ends this pass too.
        let mut dumps = Vec::with_capacity(vcpus.len() * 2);
        for vcpu in &vcpus {
            for kind in [StackKind::Current, StackKind::PageFault] {
                let mut exchanges = Vec::new();
                let dump = match kind {
                    StackKind::Current => read_current_stack(&mut monitor, &mut exchanges, vcpu),
                    StackKind::PageFault => {
                        read_page_fault_stack(&mut monitor, &mut exchanges, vcpu)
                    }
                };
                for exchange in &exchanges {
                    log.section(&exchange.title, &exchange.output)?;
                }
                if let Err(error) = &dump {
                    log.section(
                        &format!("vCPU {} {kind}", vcpu.index),
                        &format!("not captured: {error}\n"),
                    )?;
                }
                dumps.push((vcpu.index, kind, dump));
            }
        }

        let symbols = self.symbols();
        let kernel = symbols
            .as_ref()
            .ok()
            .map(|(table, base)| table.loaded_at(*base));
        let lines = vcpus
            .iter()
            .map(|registers| VcpuLine {
                registers: *registers,
                symbol: kernel
                    .as_ref()
                    .and_then(|kernel| kernel.symbolize(registers.rip)),
            })
            .collect();
        let stacks = dumps
            .into_iter()
            .map(|(vcpu, kind, dump)| StackReport {
                vcpu,
                kind,
                outcome: dump.map(|dump| StackScan::new(&dump, kernel.as_ref())),
            })
            .collect();
        Ok(CaptureReport {
            log: self.log.clone(),
            lines,
            stacks,
            lapic_failure,
            symbols: symbols.err(),
        })
    }

    /// The kernel's symbol table and the virtual base it was loaded at.
    fn symbols(&self) -> Result<(KernelSymbols, u64), SymbolizeError> {
        let serial_log = self
            .debug_serial_log
            .as_ref()
            .ok_or(SymbolizeError::NoSerialLog)?;
        let bytes = std::fs::read(serial_log).map_err(|source| SymbolizeError::ReadSerialLog {
            path: serial_log.display().to_string(),
            source,
        })?;
        let base = kernel_virtual_base(&String::from_utf8_lossy(&bytes))?.ok_or_else(|| {
            SymbolizeError::NoBaseLine {
                path: serial_log.display().to_string(),
            }
        })?;
        Ok((KernelSymbols::read(&self.kernel)?, base))
    }
}

/// The per-vCPU half of one capture's QMP conversation.
///
/// A command QEMU did not answer ends the conversation: its reply may
/// still arrive, and the next command would read it as its own, filing
/// one vCPU's state under another's name.
struct Monitor {
    qmp: QmpClient,
    /// The command that went unanswered, after which nothing is asked.
    silent_since: Option<(u32, String)>,
}

impl Monitor {
    /// Runs `command` against vCPU `vcpu`.
    fn on(&mut self, vcpu: u32, command: &str) -> Result<String, MonitorError> {
        if let Some((vcpu, command)) = &self.silent_since {
            return Err(MonitorError::Abandoned {
                vcpu: *vcpu,
                command: command.clone(),
            });
        }
        self.qmp
            .human_monitor_command_on(vcpu, command)
            .map_err(|source| {
                self.silent_since = Some((vcpu, command.to_owned()));
                MonitorError::Unanswered {
                    vcpu,
                    command: command.to_owned(),
                    source,
                }
            })
    }
}

/// One answered monitor command, kept for `vcpu-state.log`.
struct Exchange {
    title: String,
    output: String,
}

/// Reads `words` words from `start` on `vcpu`, keeping QEMU's answer in
/// `exchanges` under a title that says what the memory is.
fn read_memory(
    monitor: &mut Monitor,
    exchanges: &mut Vec<Exchange>,
    vcpu: u32,
    start: u64,
    words: u64,
    what: &str,
) -> Result<MemoryDump, StackDumpError> {
    let command = dump_command(start, words);
    let output = monitor.on(vcpu, &command)?;
    let dump = parse_memory_dump(&output, start, words);
    exchanges.push(Exchange {
        title: format!("{command} (vCPU {vcpu} {what})"),
        output,
    });
    dump.map_err(|source| StackDumpError::Dump { command, source })
}

/// 4 KiB of `vcpu`'s current stack, upward from RSP.
fn read_current_stack(
    monitor: &mut Monitor,
    exchanges: &mut Vec<Exchange>,
    vcpu: &VcpuRegisters,
) -> Result<MemoryDump, StackDumpError> {
    read_memory(
        monitor,
        exchanges,
        vcpu.index,
        vcpu.rsp,
        STACK_DUMP_WORDS,
        &StackKind::Current.to_string(),
    )
}

/// The 4 KiB below the top of `vcpu`'s page-fault IST stack, which its
/// TSS's IST1 slot holds.
fn read_page_fault_stack(
    monitor: &mut Monitor,
    exchanges: &mut Vec<Exchange>,
    vcpu: &VcpuRegisters,
) -> Result<MemoryDump, StackDumpError> {
    let slot = ist1_slot(vcpu.tr_base)?;
    let ist1 = read_memory(
        monitor,
        exchanges,
        vcpu.index,
        slot,
        1,
        &format!(
            "TSS IST1, TR base {:#x} + {TSS_IST1_OFFSET:#x}",
            vcpu.tr_base
        ),
    )?
    .first_word()
    .ok_or(StackDumpError::IstUnreadable { slot })?;
    read_memory(
        monitor,
        exchanges,
        vcpu.index,
        page_fault_stack_start(ist1)?,
        STACK_DUMP_WORDS,
        &format!("{}, the 4 KiB below IST1 {ist1:#x}", StackKind::PageFault),
    )
}

/// `vcpu-state.log`, opened for one capture.
///
/// Appended rather than replaced: a run that times out more than once
/// keeps every capture, and two captures of the same wedge say whether
/// any vCPU moved between them.
struct CaptureLog<'path> {
    path: &'path Path,
    file: File,
}

impl<'path> CaptureLog<'path> {
    fn open(path: &'path Path) -> Result<Self, CaptureError> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|source| CaptureError::WriteLog {
                path: path.display().to_string(),
                source,
            })?;
        Ok(Self { path, file })
    }

    /// Writes `body` under a `--- title ---` header, exactly as QEMU
    /// produced it.
    fn section(&mut self, title: &str, body: &str) -> Result<(), CaptureError> {
        writeln!(self.file, "--- {title} ---")
            .and_then(|()| self.file.write_all(body.as_bytes()))
            .and_then(|()| {
                if body.is_empty() || body.ends_with('\n') {
                    Ok(())
                } else {
                    writeln!(self.file)
                }
            })
            .map_err(|source| CaptureError::WriteLog {
                path: self.path.display().to_string(),
                source,
            })
    }
}

/// The registers of one vCPU that say what it was doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VcpuRegisters {
    /// QEMU's `cpu_index`, the number `CPU#<n>` prints.
    pub(crate) index: u32,
    pub(crate) rip: u64,
    pub(crate) rsp: u64,
    pub(crate) rflags: u64,
    /// The base of the task register's TSS, which holds the IST pointers.
    pub(crate) tr_base: u64,
    /// Whether the vCPU was stopped in `HLT`.
    pub(crate) halted: bool,
}

impl VcpuRegisters {
    fn interrupts_enabled(&self) -> bool {
        self.rflags & RFLAGS_INTERRUPT_ENABLE != 0
    }
}

/// Parses QEMU's `info registers -a` into one entry per vCPU.
pub(crate) fn parse_registers(dump: &str) -> Result<Vec<VcpuRegisters>, RegisterDumpError> {
    /// A vCPU section's fields as they are read.
    struct Section {
        index: u32,
        rip: Option<u64>,
        rsp: Option<u64>,
        rflags: Option<u64>,
        tr_base: Option<u64>,
        halted: Option<bool>,
    }

    impl Section {
        fn finish(self) -> Result<VcpuRegisters, RegisterDumpError> {
            let missing = |register| RegisterDumpError::Missing {
                vcpu: self.index,
                register,
            };
            Ok(VcpuRegisters {
                index: self.index,
                rip: self.rip.ok_or_else(|| missing("RIP"))?,
                rsp: self.rsp.ok_or_else(|| missing("RSP"))?,
                rflags: self.rflags.ok_or_else(|| missing("RFLAGS"))?,
                tr_base: self.tr_base.ok_or_else(|| missing(TR_BASE))?,
                halted: self.halted.ok_or_else(|| missing("HLT"))?,
            })
        }
    }

    fn store<T>(
        slot: &mut Option<T>,
        vcpu: u32,
        register: &'static str,
        value: T,
    ) -> Result<(), RegisterDumpError> {
        if slot.replace(value).is_some() {
            return Err(RegisterDumpError::Duplicate { vcpu, register });
        }
        Ok(())
    }

    let hex = |vcpu, register, text: &str| {
        u64::from_str_radix(text, 16).map_err(|_| RegisterDumpError::NotHex {
            vcpu,
            register,
            text: text.to_owned(),
        })
    };

    let mut vcpus = Vec::new();
    let mut current: Option<Section> = None;
    for line in dump.lines() {
        let line = line.trim();
        if let Some(index) = line.strip_prefix("CPU#") {
            let index = index.parse().map_err(|_| RegisterDumpError::BadHeader {
                header: line.to_owned(),
            })?;
            if let Some(section) = current.replace(Section {
                index,
                rip: None,
                rsp: None,
                rflags: None,
                tr_base: None,
                halted: None,
            }) {
                vcpus.push(section.finish()?);
            }
            continue;
        }
        let Some(section) = current.as_mut() else {
            continue;
        };
        // `TR =<selector> <base> <limit> <flags> …`: the base is the
        // second field, in long mode and out of it.
        if let Some(fields) = line
            .strip_prefix("TR")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
        {
            let vcpu = section.index;
            let base = fields
                .split_whitespace()
                .nth(1)
                .ok_or(RegisterDumpError::Missing {
                    vcpu,
                    register: TR_BASE,
                })?;
            store(
                &mut section.tr_base,
                vcpu,
                TR_BASE,
                hex(vcpu, TR_BASE, base)?,
            )?;
            continue;
        }
        for token in line.split_whitespace() {
            let Some((name, value)) = token.split_once('=') else {
                continue;
            };
            let vcpu = section.index;
            match name {
                "RIP" | "EIP" => store(&mut section.rip, vcpu, "RIP", hex(vcpu, "RIP", value)?)?,
                "RSP" | "ESP" => store(&mut section.rsp, vcpu, "RSP", hex(vcpu, "RSP", value)?)?,
                "RFL" | "EFL" => store(
                    &mut section.rflags,
                    vcpu,
                    "RFLAGS",
                    hex(vcpu, "RFLAGS", value)?,
                )?,
                "HLT" => {
                    let halted = match value {
                        "0" => false,
                        "1" => true,
                        _ => {
                            return Err(RegisterDumpError::NotAFlag {
                                vcpu,
                                text: value.to_owned(),
                            });
                        }
                    };
                    store(&mut section.halted, vcpu, "HLT", halted)?;
                }
                _ => {}
            }
        }
    }
    match current {
        Some(section) => vcpus.push(section.finish()?),
        None => {
            return Err(RegisterDumpError::NoVcpus {
                output: dump.to_owned(),
            });
        }
    }
    Ok(vcpus)
}

/// The runtime virtual base the kernel logged at boot, from the last
/// `kernel image loaded` line of a debug serial log.
///
/// The log is the raw byte copy of the line, ANSI colour included, so
/// each line is stripped before it is read. The last line wins: a guest
/// that rebooted is running the image its latest boot placed.
pub(crate) fn kernel_virtual_base(serial_log: &str) -> Result<Option<u64>, SymbolizeError> {
    let Some(line) = serial_log
        .lines()
        .rev()
        .map(strip_ansi_codes)
        .find(|line| line.contains(KERNEL_IMAGE_LOADED))
    else {
        return Ok(None);
    };
    line.split_whitespace()
        .find_map(|token| {
            token
                .strip_prefix(VIRTUAL_BASE_FIELD)?
                .strip_prefix('=')?
                .strip_prefix("0x")
        })
        .and_then(|digits| u64::from_str_radix(digits, 16).ok())
        .map(Some)
        .ok_or_else(|| SymbolizeError::MalformedBaseLine {
            line: line.trim().to_owned(),
        })
}

/// The `info lapic` that QEMU did not answer, which ended the APIC pass.
#[derive(Debug)]
struct LapicFailure {
    vcpu: u32,
    /// The vCPUs after it, whose APICs were not asked for.
    not_asked: usize,
    error: MonitorError,
}

/// One vCPU's run-log line.
#[derive(Debug)]
struct VcpuLine {
    registers: VcpuRegisters,
    symbol: Option<SymbolizedAddress>,
}

impl core::fmt::Display for VcpuLine {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let registers = &self.registers;
        write!(
            formatter,
            "vcpu {} rip={:#018x} ",
            registers.index, registers.rip
        )?;
        match &self.symbol {
            Some(symbol) => write!(formatter, "{symbol}")?,
            None => formatter.write_str("unknown")?,
        }
        write!(
            formatter,
            " interrupts={} halted={}",
            if registers.interrupts_enabled() {
                "enabled"
            } else {
                "masked"
            },
            if registers.halted { "yes" } else { "no" }
        )
    }
}

/// What one capture found, for the run log.
#[derive(Debug)]
struct CaptureReport {
    log: PathBuf,
    lines: Vec<VcpuLine>,
    /// Two per vCPU, in vCPU order: its current stack, then its
    /// page-fault IST stack.
    stacks: Vec<StackReport>,
    lapic_failure: Option<LapicFailure>,
    /// Why the lines and stacks carry no symbols, when they do not.
    symbols: Option<SymbolizeError>,
}

impl CaptureReport {
    fn print(&self) {
        eprintln!(
            "helios-inspector: vCPU state captured to {}",
            self.log.display()
        );
        for line in &self.lines {
            eprintln!("helios-inspector: {line}");
        }
        for stack in &self.stacks {
            eprintln!("helios-inspector: {stack}");
            for candidate in stack.candidates() {
                eprintln!("helios-inspector:   {candidate}");
            }
        }
        if let Some(error) = &self.symbols {
            eprintln!(
                "helios-inspector: vCPU instruction pointers and stack words not symbolised: \
                 {error}"
            );
        }
        if let Some(failure) = &self.lapic_failure {
            eprintln!(
                "helios-inspector: vCPU {} local APIC not captured, nor those of the {} vCPU(s) \
                 after it: {}",
                failure.vcpu, failure.not_asked, failure.error
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        CaptureUnavailable, RegisterDumpError, SymbolizeError, VcpuLine, VcpuRegisters,
        VcpuStateCapture, kernel_virtual_base, parse_registers,
    };
    use crate::vm::VmArch;
    use crate::workload_bench::{WorkloadBenchError, guest_step_under_deadline};

    /// `info registers -a` of a four-vCPU x86-64 guest as QEMU 8.2
    /// prints it: vCPU 0 halted with interrupts enabled, vCPU 1 spinning
    /// with them masked, vCPU 2 halted with them masked, and vCPU 3 in
    /// user mode.
    const FOUR_VCPU_DUMP: &str = include_str!("vcpu_state/info-registers-4vcpu.txt");

    #[test]
    fn every_vcpu_of_a_register_dump_is_read() {
        let vcpus = parse_registers(FOUR_VCPU_DUMP).expect("the QEMU 8.2 dump parses");
        assert_eq!(
            vcpus,
            vec![
                VcpuRegisters {
                    index: 0,
                    rip: 0xffff_ffff_8012_3456,
                    rsp: 0xffff_ffff_8041_9f58,
                    rflags: 0x246,
                    tr_base: 0xffff_8000_0f6b_0000,
                    halted: true,
                },
                VcpuRegisters {
                    index: 1,
                    rip: 0xffff_ffff_8045_6789,
                    rsp: 0xffff_ffff_8042_bf20,
                    rflags: 0x86,
                    tr_base: 0xffff_8000_0f6b_1000,
                    halted: false,
                },
                VcpuRegisters {
                    index: 2,
                    rip: 0xffff_ffff_8012_3456,
                    rsp: 0xffff_ffff_8043_7f58,
                    rflags: 0x46,
                    tr_base: 0xffff_8000_0f6b_2000,
                    halted: true,
                },
                VcpuRegisters {
                    index: 3,
                    rip: 0x0000_7f00_0010_2030,
                    rsp: 0x0000_7f00_003f_fe90,
                    rflags: 0x202,
                    tr_base: 0xffff_8000_0f6b_3000,
                    halted: false,
                },
            ]
        );
        let interrupts: Vec<bool> = vcpus
            .iter()
            .map(VcpuRegisters::interrupts_enabled)
            .collect();
        assert_eq!(interrupts, [true, false, false, true]);
    }

    #[test]
    fn a_vcpu_line_names_the_function_and_the_interrupt_flag() {
        let line = VcpuLine {
            registers: VcpuRegisters {
                index: 1,
                rip: 0xffff_ffff_8045_6789,
                rsp: 0xffff_ffff_8042_bf20,
                rflags: 0x86,
                tr_base: 0xffff_8000_0f6b_1000,
                halted: false,
            },
            symbol: Some(super::SymbolizedAddress {
                function: "helios_x86::smp::wait_for_ack".to_owned(),
                offset: 0x19,
            }),
        };
        assert_eq!(
            line.to_string(),
            "vcpu 1 rip=0xffffffff80456789 helios_x86::smp::wait_for_ack+0x19 \
             interrupts=masked halted=no"
        );
        let unnamed = VcpuLine {
            symbol: None,
            ..line
        };
        assert!(unnamed.to_string().contains(" unknown interrupts=masked"));
    }

    #[test]
    fn a_dump_that_is_not_a_register_dump_is_refused() {
        assert_eq!(
            parse_registers("unknown command: 'info registerz'\r\n"),
            Err(RegisterDumpError::NoVcpus {
                output: "unknown command: 'info registerz'\r\n".to_owned(),
            })
        );
        assert_eq!(
            parse_registers("\r\nCPU#0\r\nRAX=0000000000000000\r\n"),
            Err(RegisterDumpError::Missing {
                vcpu: 0,
                register: "RIP",
            })
        );
        assert!(matches!(
            parse_registers("CPU#0\nRIP=zzzz RFL=00000002 [-------] CPL=0 HLT=0\n"),
            Err(RegisterDumpError::NotHex {
                vcpu: 0,
                register: "RIP",
                ..
            })
        ));
    }

    /// The stack capture needs RSP and the TSS base; a dump without
    /// them, or with them twice, says which.
    #[test]
    fn rsp_and_the_tss_base_are_required_once_per_vcpu() {
        const REGISTERS: &str = "RSP=ffffffff80419f58\n\
                                 RIP=ffffffff80123456 RFL=00000246 [---Z-P-] CPL=0 HLT=1\n";
        const TR: &str = "TR =0028 ffff80000f6b0000 00000067 00008900 DPL=0 TSS64-avl\n";
        let vcpu =
            parse_registers(&format!("CPU#0\n{REGISTERS}{TR}")).expect("a complete section parses");
        assert_eq!(vcpu[0].rsp, 0xffff_ffff_8041_9f58);
        assert_eq!(vcpu[0].tr_base, 0xffff_8000_0f6b_0000);
        assert_eq!(
            parse_registers(&format!("CPU#0\n{REGISTERS}")),
            Err(RegisterDumpError::Missing {
                vcpu: 0,
                register: super::TR_BASE,
            })
        );
        assert_eq!(
            parse_registers(&format!("CPU#0\n{REGISTERS}{TR}{TR}")),
            Err(RegisterDumpError::Duplicate {
                vcpu: 0,
                register: super::TR_BASE,
            })
        );
        assert_eq!(
            parse_registers(&format!(
                "CPU#0\nRIP=ffffffff80123456 RFL=00000246 CPL=0 HLT=1\n{TR}"
            )),
            Err(RegisterDumpError::Missing {
                vcpu: 0,
                register: "RSP",
            })
        );
        assert!(matches!(
            parse_registers(&format!(
                "CPU#0\n{REGISTERS}TR =0028 ffff80000f6bzzzz 00000067 00008900\n"
            )),
            Err(RegisterDumpError::NotHex {
                vcpu: 0,
                register: super::TR_BASE,
                ..
            })
        ));
        // A `TR` line cut short after its selector carries no base.
        assert_eq!(
            parse_registers(&format!("CPU#0\n{REGISTERS}TR =0028\n")),
            Err(RegisterDumpError::Missing {
                vcpu: 0,
                register: super::TR_BASE,
            })
        );
    }

    /// The debug serial log is the raw line: the kernel's level colour
    /// codes are in it, and the base line sits among the other boot
    /// lines.
    #[test]
    fn the_load_base_is_read_from_a_coloured_serial_log() {
        let log = "\u{1b}[1;32mINFO \u{1b}[0m [helios_kernel] memory regions primed\r\n\
                   \u{1b}[1;32mINFO \u{1b}[0m [helios_x86] kernel image loaded \
                   virtual_base=0xffffffff81200000 physical_base=0x3e00000 \r\n\
                   \u{1b}[1;33mWARN \u{1b}[0m [helios_x86] virtio 9p device was not discovered\r\n";
        assert_eq!(
            kernel_virtual_base(log).expect("the line is well formed"),
            Some(0xffff_ffff_8120_0000)
        );
        // The field order of a tracing event is not the contract; the
        // field name is.
        let fields_first = "\u{1b}[1;32mINFO \u{1b}[0m [helios_x86] \
                            physical_base=0x3e00000 virtual_base=0xffffffff80000000 \
                            kernel image loaded\n";
        assert_eq!(
            kernel_virtual_base(fields_first).expect("the line is well formed"),
            Some(0xffff_ffff_8000_0000)
        );
        assert_eq!(
            kernel_virtual_base("INFO  [helios_x86] booting\n").expect("no line is not an error"),
            None
        );
        assert!(matches!(
            kernel_virtual_base("INFO  [helios_x86] kernel image loaded virtual_base=18446\n"),
            Err(SymbolizeError::MalformedBaseLine { .. })
        ));
    }

    #[test]
    fn only_an_x86_session_with_a_qmp_socket_can_capture() {
        let runtime = std::path::Path::new("/nonexistent/runtime");
        let kernel = std::path::PathBuf::from("/nonexistent/kernel");
        let no_socket = VcpuStateCapture::new(VmArch::X86_64, None, runtime, None, kernel.clone());
        assert!(matches!(
            no_socket.probe,
            Err(CaptureUnavailable::NoQmpSocket)
        ));
        let riscv = VcpuStateCapture::new(
            VmArch::Riscv64,
            Some(runtime.join("qmp.sock")),
            runtime,
            None,
            kernel,
        );
        assert!(matches!(
            riscv.probe,
            Err(CaptureUnavailable::Architecture { .. })
        ));
    }

    /// A capture that cannot reach QEMU is reported beside the timeout
    /// and never in place of it: the run still fails with the step that
    /// did not answer.
    #[test]
    fn a_failed_capture_still_surfaces_the_timeout() {
        let runtime = tempfile::tempdir().expect("a scratch runtime directory");
        let capture = VcpuStateCapture::new(
            VmArch::X86_64,
            Some(runtime.path().join("no-qemu-is-listening.sock")),
            runtime.path(),
            None,
            runtime.path().join("kernel"),
        );
        let timed_out = crate::runtime::block_on(guest_step_under_deadline(
            "the tracing fetch",
            1,
            &capture,
            async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                Ok::<(), WorkloadBenchError>(())
            },
        ))
        .expect_err("a step that outlives its deadline fails");
        assert!(
            matches!(
                timed_out,
                WorkloadBenchError::GuestStepTimedOut {
                    step: "the tracing fetch",
                    seconds: 1,
                }
            ),
            "the timeout must survive the failed capture, got {timed_out}"
        );
        assert!(
            !runtime.path().join(super::VCPU_STATE_LOG_NAME).exists(),
            "a capture that never reached QEMU writes no log"
        );
    }
}
