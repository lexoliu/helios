//! The two stacks of each vCPU an abandoned interrupt handler leaves
//! its trace on, read back through the monitor's `x` command.
//!
//! An interrupted handler leaves its `iretq` frame and the return
//! address of every call it made on the stack it ran on: the vCPU's
//! current stack, or the page-fault IST stack every page fault starts
//! from and which an interrupt taken while a page fault is handled with
//! interrupts re-enabled shares. The release kernel keeps no frame
//! pointers, so neither stack can be walked. What the capture can do is
//! list every word of a dump that falls inside the kernel's executable
//! segments: each is a *candidate* return address, since a stale spill
//! or a function pointer in a local reads the same.

use core::fmt;

use super::MonitorError;
use crate::vm::kernel_image::{LoadedKernelSymbols, SymbolizedAddress};

/// The size of one stack word, and of the unit `x /<n>gx` reads.
const WORD_BYTES: u64 = 8;

/// How much of each stack the capture reads.
const STACK_DUMP_BYTES: u64 = 4096;

/// [`STACK_DUMP_BYTES`] in words, the count every stack dump asks for.
pub(super) const STACK_DUMP_WORDS: u64 = STACK_DUMP_BYTES / WORD_BYTES;

/// The offset of IST1 in the 64-bit TSS: a reserved doubleword, RSP0 to
/// RSP2, a reserved quadword, then IST1 (Intel SDM Vol. 3A, "64-Bit TSS
/// Format"). The x86 backend installs the page-fault stack's top there
/// (`PAGE_FAULT_IST_INDEX`, `x86/src/exceptions.rs`).
pub(super) const TSS_IST1_OFFSET: u64 = 0x24;

/// What QEMU prints in place of the words of a line it cannot read
/// (`memory_dump`, `monitor/hmp-cmds-target.c`), after which it stops.
const INACCESSIBLE: &str = "Cannot access memory";

/// The monitor command that reads `words` 8-byte words of the current
/// vCPU's virtual memory upward from `start`.
pub(super) fn dump_command(start: u64, words: u64) -> String {
    format!("x /{words}gx {start:#x}")
}

/// The address of the TSS slot holding IST1, for a TSS at `tr_base`.
pub(super) fn ist1_slot(tr_base: u64) -> Result<u64, StackDumpError> {
    tr_base
        .checked_add(TSS_IST1_OFFSET)
        .ok_or(StackDumpError::TssPastAddressSpace { tr_base })
}

/// Where the dump of the page-fault stack starts: the 4 KiB below its
/// top, which is where the frames of the latest page fault sit.
pub(super) fn page_fault_stack_start(ist1: u64) -> Result<u64, StackDumpError> {
    ist1.checked_sub(STACK_DUMP_BYTES)
        .ok_or(StackDumpError::NoPageFaultStack { ist1 })
}

/// Why one stack of one vCPU was not dumped.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StackDumpError {
    #[error("{source}")]
    Monitor {
        #[from]
        source: MonitorError,
    },
    #[error("`{command}` was not answered with a memory dump: {source}")]
    Dump {
        command: String,
        #[source]
        source: MemoryDumpError,
    },
    #[error("the TR base {tr_base:#x} puts the TSS's IST1 slot past the end of the address space")]
    TssPastAddressSpace { tr_base: u64 },
    #[error("the TSS's IST1 slot at {slot:#x} cannot be read")]
    IstUnreadable { slot: u64 },
    #[error("IST1 reads {ist1:#x}, which leaves no 4 KiB below it for a page-fault stack")]
    NoPageFaultStack { ist1: u64 },
}

/// Why the text QEMU answered an `x /<n>gx` with is not the dump that
/// command prints.
///
/// The format is QEMU's `memory_dump`: per line, the address of its
/// first word as 16 hexadecimal digits and a colon, then up to two
/// words as ` 0x` and 16 digits each; or the address and
/// ` Cannot access memory`, after which the dump stops.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MemoryDumpError {
    #[error("{line:?} is not a `<address>: <words>` dump line")]
    NotADumpLine { line: String },
    #[error("{line:?} starts at a different address than the {expected:#x} the dump had reached")]
    Discontiguous { line: String, expected: u64 },
    #[error("{line:?} carries {word:?}, which is not a `0x`-prefixed hexadecimal word")]
    NotAWord { line: String, word: String },
    #[error("the dump from {start:#x} runs past the end of the address space")]
    PastAddressSpace { start: u64 },
    #[error("{line:?} follows the line QEMU could not read, after which it prints nothing")]
    AfterInaccessible { line: String },
    #[error("the dump holds {read} words where {asked} were asked for")]
    Overrun { read: u64, asked: u64 },
    #[error("the dump stops after {read} of {asked} words without saying memory was unreadable")]
    Short { read: u64, asked: u64 },
}

/// Consecutive 8-byte words of a vCPU's virtual memory, as `x /<n>gx`
/// read them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MemoryDump {
    start: u64,
    /// The words from `start` upward. Parsing checked that the address
    /// of each fits in the address space.
    words: Vec<u64>,
    /// Where the dump stopped short because QEMU could not read the
    /// memory there.
    inaccessible_at: Option<u64>,
}

impl MemoryDump {
    /// Every word with the address it was read from.
    pub(super) fn words(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.words
            .iter()
            .enumerate()
            .map(|(position, &value)| (self.start + position as u64 * WORD_BYTES, value))
    }

    /// The first word, when QEMU could read it.
    pub(super) fn first_word(&self) -> Option<u64> {
        self.words.first().copied()
    }
}

/// Parses QEMU's answer to `x /<words>gx <start>`.
///
/// A dump that stops at memory QEMU cannot read is a dump, not an
/// error: four kilobytes upward from a stack pointer close to its top
/// run into the guard page above it.
pub(super) fn parse_memory_dump(
    output: &str,
    start: u64,
    words: u64,
) -> Result<MemoryDump, MemoryDumpError> {
    let mut dump = MemoryDump {
        start,
        words: Vec::new(),
        inaccessible_at: None,
    };
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if dump.inaccessible_at.is_some() {
            return Err(MemoryDumpError::AfterInaccessible {
                line: line.to_owned(),
            });
        }
        let not_a_line = || MemoryDumpError::NotADumpLine {
            line: line.to_owned(),
        };
        let (address, rest) = line.split_once(':').ok_or_else(not_a_line)?;
        let address = u64::from_str_radix(address, 16).map_err(|_| not_a_line())?;
        let expected = (dump.words.len() as u64)
            .checked_mul(WORD_BYTES)
            .and_then(|offset| start.checked_add(offset))
            .ok_or(MemoryDumpError::PastAddressSpace { start })?;
        if address != expected {
            return Err(MemoryDumpError::Discontiguous {
                line: line.to_owned(),
                expected,
            });
        }
        let rest = rest.trim();
        if rest == INACCESSIBLE {
            dump.inaccessible_at = Some(address);
            continue;
        }
        for word in rest.split_whitespace() {
            let value = word
                .strip_prefix("0x")
                .and_then(|digits| u64::from_str_radix(digits, 16).ok())
                .ok_or_else(|| MemoryDumpError::NotAWord {
                    line: line.to_owned(),
                    word: word.to_owned(),
                })?;
            dump.words.push(value);
        }
    }
    let read = dump.words.len() as u64;
    if read > words {
        return Err(MemoryDumpError::Overrun { read, asked: words });
    }
    if read < words && dump.inaccessible_at.is_none() {
        return Err(MemoryDumpError::Short { read, asked: words });
    }
    Ok(dump)
}

/// A word of a stack dump that falls inside the kernel's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Candidate {
    /// Where on the stack the word sits.
    pub(super) stack_address: u64,
    pub(super) value: u64,
    /// The function containing `value`; `None` for text no function
    /// symbol covers (padding, a trampoline without a sized symbol).
    pub(super) symbol: Option<SymbolizedAddress>,
}

impl fmt::Display for Candidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:#018x}: {:#018x} ",
            self.stack_address, self.value
        )?;
        match &self.symbol {
            Some(symbol) => write!(formatter, "{symbol}"),
            None => formatter.write_str("in kernel text, outside every function symbol"),
        }
    }
}

/// Every word of `dump` that lies in the kernel's executable segments,
/// in stack order, named by the function containing it.
pub(super) fn return_address_candidates(
    dump: &MemoryDump,
    kernel: &LoadedKernelSymbols<'_>,
) -> Vec<Candidate> {
    dump.words()
        .filter(|&(_, value)| kernel.contains_text(value))
        .map(|(stack_address, value)| Candidate {
            stack_address,
            value,
            symbol: kernel.symbolize(value),
        })
        .collect()
}

/// Which of a vCPU's stacks a dump covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StackKind {
    /// 4 KiB upward from RSP.
    Current,
    /// The 4 KiB below IST1, the page-fault stack's top.
    PageFault,
}

impl fmt::Display for StackKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Current => "stack from RSP",
            Self::PageFault => "page-fault IST stack",
        })
    }
}

/// What one stack dump showed, for the run log.
#[derive(Debug)]
pub(super) struct StackScan {
    start: u64,
    words: u64,
    inaccessible_at: Option<u64>,
    /// `None` when the kernel's symbols could not be read; the capture
    /// reports why once, beside the instruction pointers it could not
    /// name either.
    candidates: Option<Vec<Candidate>>,
}

impl StackScan {
    pub(super) fn new(dump: &MemoryDump, kernel: Option<&LoadedKernelSymbols<'_>>) -> Self {
        Self {
            start: dump.start,
            words: dump.words.len() as u64,
            inaccessible_at: dump.inaccessible_at,
            candidates: kernel.map(|kernel| return_address_candidates(dump, kernel)),
        }
    }
}

/// One stack of one vCPU, as the run log reports it.
#[derive(Debug)]
pub(super) struct StackReport {
    pub(super) vcpu: u32,
    pub(super) kind: StackKind,
    pub(super) outcome: Result<StackScan, StackDumpError>,
}

impl StackReport {
    /// The candidates to list under the header, one per run-log line.
    pub(super) fn candidates(&self) -> &[Candidate] {
        match &self.outcome {
            Ok(StackScan {
                candidates: Some(candidates),
                ..
            }) => candidates,
            Ok(StackScan {
                candidates: None, ..
            })
            | Err(_) => &[],
        }
    }
}

impl fmt::Display for StackReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            vcpu,
            kind,
            outcome,
        } = self;
        let scan = match outcome {
            Ok(scan) => scan,
            Err(error) => return write!(formatter, "vcpu {vcpu} {kind} not dumped: {error}"),
        };
        let end = scan.start + scan.words * WORD_BYTES;
        write!(
            formatter,
            "vcpu {vcpu} {kind} {:#x}..{end:#x}: ",
            scan.start
        )?;
        match &scan.candidates {
            Some(candidates) => write!(
                formatter,
                "{} candidate return address(es), the words in kernel text (no frame \
                 pointers: not a verified backtrace)",
                candidates.len()
            )?,
            None => write!(
                formatter,
                "{} words dumped, not scanned for return addresses without kernel symbols",
                scan.words
            )?,
        }
        if let Some(address) = scan.inaccessible_at {
            write!(
                formatter,
                "; the dump stops at {address:#x}, which QEMU cannot read"
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Candidate, MemoryDumpError, STACK_DUMP_WORDS, StackDumpError, StackKind, StackReport,
        StackScan, dump_command, ist1_slot, page_fault_stack_start, parse_memory_dump,
        return_address_candidates,
    };
    use crate::vm::kernel_image::{Function, KernelSymbols, SymbolizedAddress};

    /// `x /512gx 0xffffffff8042bf20` on vCPU 1 of a four-vCPU guest as
    /// QEMU 8.2 prints it: RSP sits 0xe0 bytes below the top of its
    /// stack, so the dump runs into the unmapped guard page above it
    /// after 28 words. The stack holds an interrupt frame (vector 0x41,
    /// RIP, CS 0x8, RFLAGS, RSP, SS 0x10), pointers into the kernel's
    /// data, and words at both edges of its text.
    const STACK_DUMP: &str = include_str!("x-512gx-rsp-vcpu1.txt");
    const STACK_DUMP_START: u64 = 0xffff_ffff_8042_bf20;

    fn function(name: &str, address: u64, size: u64) -> Function {
        Function {
            name: name.to_owned(),
            address,
            size,
        }
    }

    /// A kernel linked at the canonical higher-half base, its text one
    /// executable segment from 0xffffffff80001000 to 0xffffffff80300000.
    fn kernel() -> KernelSymbols {
        let text = 0xffff_ffff_8000_1000..0xffff_ffff_8030_0000;
        KernelSymbols::new(
            0xffff_ffff_8000_0000,
            vec![text],
            vec![
                function("_start", 0xffff_ffff_8000_1000, 0x40),
                function("wait_for_ack", 0xffff_ffff_800c_c200, 0x100),
                function("idle_park", 0xffff_ffff_8012_3400, 0x100),
                function("apic_timer_interrupt", 0xffff_ffff_8020_1a00, 0x100),
            ],
        )
    }

    fn symbol(function: &str, offset: u64) -> Option<SymbolizedAddress> {
        Some(SymbolizedAddress {
            function: function.to_owned(),
            offset,
        })
    }

    #[test]
    fn the_dump_command_reads_words_upward_from_its_start() {
        assert_eq!(
            dump_command(STACK_DUMP_START, STACK_DUMP_WORDS),
            "x /512gx 0xffffffff8042bf20"
        );
        assert_eq!(
            dump_command(0xffff_8000_0f6b_1024, 1),
            "x /1gx 0xffff80000f6b1024"
        );
    }

    #[test]
    fn a_dump_that_runs_into_unmapped_memory_keeps_what_it_read() {
        let dump = parse_memory_dump(STACK_DUMP, STACK_DUMP_START, STACK_DUMP_WORDS)
            .expect("the QEMU 8.2 dump parses");
        assert_eq!(dump.words.len(), 28);
        assert_eq!(dump.inaccessible_at, Some(0xffff_ffff_8042_c000));
        let words: Vec<(u64, u64)> = dump.words().collect();
        assert_eq!(words[0], (0xffff_ffff_8042_bf20, 0));
        assert_eq!(words[1], (0xffff_ffff_8042_bf28, 0xffff_ffff_8012_3456));
        assert_eq!(words[27], (0xffff_ffff_8042_bff8, 0));
    }

    #[test]
    fn a_complete_dump_is_read_whatever_its_line_endings() {
        let output = "ffffffff80419f58: 0x0000000000000000 0xffffffff80123456\r\n\
                      ffffffff80419f68: 0x0000000000000246 0x0000000000000010\r\n";
        let dump =
            parse_memory_dump(output, 0xffff_ffff_8041_9f58, 4).expect("a complete dump parses");
        assert_eq!(
            dump.words().collect::<Vec<_>>(),
            [
                (0xffff_ffff_8041_9f58, 0),
                (0xffff_ffff_8041_9f60, 0xffff_ffff_8012_3456),
                (0xffff_ffff_8041_9f68, 0x246),
                (0xffff_ffff_8041_9f70, 0x10),
            ]
        );
        assert_eq!(dump.inaccessible_at, None);
        // An odd count leaves one word on the last line.
        let one = parse_memory_dump(
            "ffff80000f6b1024: 0xffff800010010000\n",
            0xffff_8000_0f6b_1024,
            1,
        )
        .expect("a one-word dump parses");
        assert_eq!(one.first_word(), Some(0xffff_8000_1001_0000));
    }

    #[test]
    fn text_that_is_not_the_dump_asked_for_is_refused() {
        assert_eq!(
            parse_memory_dump("Can not dump without CPU\n", 0x1000, 2),
            Err(MemoryDumpError::NotADumpLine {
                line: "Can not dump without CPU".to_owned(),
            })
        );
        assert_eq!(
            parse_memory_dump("0000000000002000: 0x0000000000000000\n", 0x1000, 1),
            Err(MemoryDumpError::Discontiguous {
                line: "0000000000002000: 0x0000000000000000".to_owned(),
                expected: 0x1000,
            })
        );
        assert_eq!(
            parse_memory_dump("0000000000001000: 0000000000000000\n", 0x1000, 1),
            Err(MemoryDumpError::NotAWord {
                line: "0000000000001000: 0000000000000000".to_owned(),
                word: "0000000000000000".to_owned(),
            })
        );
        assert_eq!(
            parse_memory_dump(
                "0000000000001000: 0x0000000000000000 0x0000000000000001\n",
                0x1000,
                1
            ),
            Err(MemoryDumpError::Overrun { read: 2, asked: 1 })
        );
        assert_eq!(
            parse_memory_dump("0000000000001000: 0x0000000000000000\n", 0x1000, 2),
            Err(MemoryDumpError::Short { read: 1, asked: 2 })
        );
        assert_eq!(
            parse_memory_dump("", 0x1000, 2),
            Err(MemoryDumpError::Short { read: 0, asked: 2 })
        );
        assert_eq!(
            parse_memory_dump(
                "0000000000001000: Cannot access memory\n\
                 0000000000001000: 0x0000000000000000\n",
                0x1000,
                2
            ),
            Err(MemoryDumpError::AfterInaccessible {
                line: "0000000000001000: 0x0000000000000000".to_owned(),
            })
        );
    }

    #[test]
    fn ist1_is_read_from_the_tss_and_its_stack_is_the_4_kib_below() {
        // vCPU 1's TR base in the register dump fixture.
        assert_eq!(
            ist1_slot(0xffff_8000_0f6b_1000).expect("the slot is addressable"),
            0xffff_8000_0f6b_1024
        );
        assert!(matches!(
            ist1_slot(u64::MAX - 0x10),
            Err(StackDumpError::TssPastAddressSpace { .. })
        ));
        // A 16-byte-aligned top, as the x86 backend asserts it is.
        assert_eq!(
            page_fault_stack_start(0xffff_8000_1001_0000).expect("a real top"),
            0xffff_8000_1000_f000
        );
        // A processor whose TSS was never installed reads IST1 as zero.
        assert!(matches!(
            page_fault_stack_start(0),
            Err(StackDumpError::NoPageFaultStack { ist1: 0 })
        ));
    }

    #[test]
    fn only_words_in_kernel_text_are_candidates() {
        let kernel = kernel();
        let loaded = kernel.loaded_at(0xffff_ffff_8000_0000);
        let dump = parse_memory_dump(STACK_DUMP, STACK_DUMP_START, STACK_DUMP_WORDS)
            .expect("the QEMU 8.2 dump parses");
        // Not candidates: zeros, the vector, CS, RFLAGS, SS, the saved
        // RSP and the per-CPU pointers (kernel data and the direct map),
        // one byte below the text, and one past its end.
        assert_eq!(
            return_address_candidates(&dump, &loaded),
            [
                Candidate {
                    stack_address: 0xffff_ffff_8042_bf28,
                    value: 0xffff_ffff_8012_3456,
                    symbol: symbol("idle_park", 0x56),
                },
                Candidate {
                    stack_address: 0xffff_ffff_8042_bf40,
                    value: 0xffff_ffff_8020_1a7c,
                    symbol: symbol("apic_timer_interrupt", 0x7c),
                },
                Candidate {
                    stack_address: 0xffff_ffff_8042_bf78,
                    value: 0xffff_ffff_8012_3400,
                    symbol: symbol("idle_park", 0),
                },
                Candidate {
                    stack_address: 0xffff_ffff_8042_bf90,
                    value: 0xffff_ffff_800c_c289,
                    symbol: symbol("wait_for_ack", 0x89),
                },
                Candidate {
                    stack_address: 0xffff_ffff_8042_bfb0,
                    value: 0xffff_ffff_802f_ffff,
                    symbol: None,
                },
                Candidate {
                    stack_address: 0xffff_ffff_8042_bff0,
                    value: 0xffff_ffff_8000_1000,
                    symbol: symbol("_start", 0),
                },
            ]
        );
    }

    #[test]
    fn a_stack_report_names_its_candidates_and_where_the_dump_stopped() {
        let kernel = kernel();
        let loaded = kernel.loaded_at(0xffff_ffff_8000_0000);
        let dump = parse_memory_dump(STACK_DUMP, STACK_DUMP_START, STACK_DUMP_WORDS)
            .expect("the QEMU 8.2 dump parses");
        let report = StackReport {
            vcpu: 1,
            kind: StackKind::Current,
            outcome: Ok(StackScan::new(&dump, Some(&loaded))),
        };
        assert_eq!(
            report.to_string(),
            "vcpu 1 stack from RSP 0xffffffff8042bf20..0xffffffff8042c000: 6 candidate return \
             address(es), the words in kernel text (no frame pointers: not a verified \
             backtrace); the dump stops at 0xffffffff8042c000, which QEMU cannot read"
        );
        assert_eq!(
            report.candidates()[1].to_string(),
            "0xffffffff8042bf40: 0xffffffff80201a7c apic_timer_interrupt+0x7c"
        );
        assert_eq!(
            report.candidates()[4].to_string(),
            "0xffffffff8042bfb0: 0xffffffff802fffff in kernel text, outside every function symbol"
        );
        let unscanned = StackReport {
            vcpu: 1,
            kind: StackKind::Current,
            outcome: Ok(StackScan::new(&dump, None)),
        };
        assert!(unscanned.candidates().is_empty());
        assert!(
            unscanned
                .to_string()
                .contains(": 28 words dumped, not scanned for return addresses")
        );
        let failed = StackReport {
            vcpu: 2,
            kind: StackKind::PageFault,
            outcome: Err(StackDumpError::NoPageFaultStack { ist1: 0 }),
        };
        assert_eq!(
            failed.to_string(),
            "vcpu 2 page-fault IST stack not dumped: IST1 reads 0x0, which leaves no 4 KiB \
             below it for a page-fault stack"
        );
    }
}
