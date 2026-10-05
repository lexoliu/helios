//! The x86 kernel image's instruction-encoding audit.
//!
//! The x86 kernel saves only x87/SSE state on interrupt entry
//! (`x86/src/extended_state.rs`), which is correct only while the kernel
//! itself never writes YMM, ZMM or opmask state. The audit decodes every
//! executable section of the built image and refuses any instruction that is
//! not legacy-encoded (VEX, EVEX, XOP, 3DNow!, MVEX) or does not decode at
//! all. The secondary-wakeup trampoline is decoded in the mode each of its
//! parts runs in, and its GDT and data tail are not code.
//!
//! Release and profile-use images are built from the same source as
//! profile-generate, and link-time dead-code elimination retains everything
//! reachable from a call path, so the release audit covers any code
//! profile-generate can execute. Profile-generate only collects profiles; it
//! never ships or is measured, and its `__llvm_prf_data` deliberately keeps
//! uncalled functions alive, so it is not audited. The exemption is selected
//! only by the build profile (`KernelBuildProfile`), never by a symbol or
//! section heuristic.

use std::ops::Range;
use std::path::Path;

use iced_x86::{Code, Decoder, DecoderOptions, EncodingKind};
use object::{Object as _, ObjectSection as _, ObjectSymbol as _, SectionFlags};

use super::kernel_image::{self, KernelSymbols, KernelSymbolsError, LoadedKernelSymbols};

/// The trampoline's mode boundaries, in address order: 16-bit code from the
/// start, 32-bit from protected-mode entry, 64-bit from long-mode entry, and
/// the GDT and data from `gdt` to `end`.
const TRAMPOLINE_SYMBOLS: [&str; 5] = [
    "helios_x86_secondary_wakeup_start",
    "helios_x86_secondary_wakeup_protected_mode_start",
    "helios_x86_secondary_wakeup_long_mode_start",
    "helios_x86_secondary_wakeup_gdt",
    "helios_x86_secondary_wakeup_end",
];

/// How many rejected instructions the error lists.
const REPORTED_HITS: usize = 10;

#[derive(Debug, thiserror::Error)]
pub(crate) enum X86ImageAuditError {
    #[error("{0}")]
    Image(#[from] KernelSymbolsError),
    #[error("failed to read section {section} of x86 kernel image {path}: {source}")]
    SectionData {
        path: String,
        section: String,
        #[source]
        source: object::Error,
    },
    #[error("x86 kernel image {path} is missing secondary-wakeup symbol {symbol}")]
    TrampolineSymbol { path: String, symbol: &'static str },
    #[error(
        "x86 kernel image {path} has invalid secondary-wakeup symbol order: \
         {start:#x}, {protected_mode_start:#x}, {long_mode_start:#x}, {gdt:#x}, {end:#x}"
    )]
    TrampolineLayout {
        path: String,
        start: u64,
        protected_mode_start: u64,
        long_mode_start: u64,
        gdt: u64,
        end: u64,
    },
    #[error(
        "x86 kernel image {path} contains {count} VEX/EVEX-encoded or undecodable instructions \
         in {instructions} decoded; the kernel saves only x87/SSE state on interrupt entry, so it \
         must never execute AVX code (x86/src/extended_state.rs). First: {first}"
    )]
    Rejected {
        path: String,
        count: usize,
        instructions: usize,
        first: String,
    },
}

/// One instruction the audit refused.
#[derive(Debug)]
struct Hit {
    ip: u64,
    code: Code,
    encoding: EncodingKind,
    section: String,
    bytes: String,
}

/// Audits the x86 kernel ELF at `path`, returning how many instructions it
/// decoded.
pub(crate) fn audit(path: &Path) -> Result<usize, X86ImageAuditError> {
    let display = || path.display().to_string();
    let bytes = kernel_image::read_image(path)?;
    let image = kernel_image::parse_image(path, &bytes)?;
    let functions = KernelSymbols::from_image(path, &image)?;
    let mut boundaries = [None; TRAMPOLINE_SYMBOLS.len()];
    for symbol in image.symbols().filter(|symbol| !symbol.is_undefined()) {
        let Ok(name) = symbol.name() else { continue };
        if let Some(index) = TRAMPOLINE_SYMBOLS.iter().position(|wanted| *wanted == name) {
            boundaries[index] = Some(symbol.address());
        }
    }
    let mut addresses = [0; TRAMPOLINE_SYMBOLS.len()];
    for ((address, boundary), symbol) in
        addresses.iter_mut().zip(boundaries).zip(TRAMPOLINE_SYMBOLS)
    {
        *address = boundary.ok_or_else(|| X86ImageAuditError::TrampolineSymbol {
            path: display(),
            symbol,
        })?;
    }
    let [start, protected_mode_start, long_mode_start, gdt, end] = addresses;
    if !addresses.is_sorted_by(|earlier, later| earlier < later) {
        return Err(X86ImageAuditError::TrampolineLayout {
            path: display(),
            start,
            protected_mode_start,
            long_mode_start,
            gdt,
            end,
        });
    }

    let mut instructions = 0;
    let mut hits = Vec::new();
    for section in image.sections() {
        let executable = matches!(
            section.flags(),
            SectionFlags::Elf { sh_flags } if sh_flags & u64::from(object::elf::SHF_EXECINSTR) != 0
        );
        if !executable {
            continue;
        }
        let section_name = section.name().unwrap_or("<unnamed>").to_owned();
        let data = section
            .data()
            .map_err(|source| X86ImageAuditError::SectionData {
                path: display(),
                section: section_name.clone(),
                source,
            })?;
        let section_start = section.address();
        let ranges = [
            (section_start..start, 64),
            (start..protected_mode_start, 16),
            (protected_mode_start..long_mode_start, 32),
            (long_mode_start..gdt, 64),
            (end..u64::MAX, 64),
        ];
        for (range, bitness) in ranges {
            let Some(code) = clip(data, section_start, range) else {
                continue;
            };
            let (count, flagged) = audit_encodings(code.bytes, code.start, bitness);
            instructions += count;
            hits.extend(flagged.into_iter().map(|(ip, code, encoding)| {
                let offset = usize::try_from(ip - section_start)
                    .expect("an instruction of a section lies within its data");
                Hit {
                    ip,
                    code,
                    encoding,
                    section: section_name.clone(),
                    bytes: data[offset..data.len().min(offset + 16)]
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                }
            }));
        }
    }
    if !hits.is_empty() {
        let linked = functions.linked();
        return Err(X86ImageAuditError::Rejected {
            path: display(),
            count: hits.len(),
            instructions,
            first: hits
                .iter()
                .take(REPORTED_HITS)
                .map(|hit| describe(hit, &linked))
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    eprintln!("audited x86 kernel: {instructions} instructions, all legacy-encoded");
    Ok(instructions)
}

/// The bytes of a section that fall inside an address range.
struct Clipped<'data> {
    start: u64,
    bytes: &'data [u8],
}

fn clip(data: &[u8], section_start: u64, range: Range<u64>) -> Option<Clipped<'_>> {
    let section_end = section_start + data.len() as u64;
    let start = range.start.max(section_start);
    let end = range.end.min(section_end);
    (start < end).then(|| {
        let offset = usize::try_from(start - section_start).expect("section offset fits usize");
        let length = usize::try_from(end - start).expect("section range fits usize");
        Clipped {
            start,
            bytes: &data[offset..offset + length],
        }
    })
}

fn describe(hit: &Hit, functions: &LoadedKernelSymbols<'_>) -> String {
    let symbol = functions.symbolize(hit.ip).map_or_else(
        || "<no function symbol>".to_owned(),
        |symbol| symbol.to_string(),
    );
    format!(
        "{:#x} {} {symbol} {:?} {:?} bytes=[{}]",
        hit.ip, hit.section, hit.code, hit.encoding, hit.bytes
    )
}

/// Decodes `code`, linked at `ip`, as `bitness`-bit code: how many
/// instructions it holds and each one that is undecodable or not
/// legacy-encoded.
fn audit_encodings(code: &[u8], ip: u64, bitness: u32) -> (usize, Vec<(u64, Code, EncodingKind)>) {
    let mut decoder = Decoder::with_ip(bitness, code, ip, DecoderOptions::NONE);
    let mut instructions = 0;
    let mut hits = Vec::new();
    while decoder.can_decode() {
        let instruction = decoder.decode();
        instructions += 1;
        let code = instruction.code();
        if code == Code::INVALID || code.encoding() != EncodingKind::Legacy {
            hits.push((instruction.ip(), code, code.encoding()));
        }
    }
    (instructions, hits)
}

#[cfg(test)]
mod tests {
    use iced_x86::{Code, EncodingKind};

    use super::{audit_encodings, clip};

    #[test]
    fn vex_and_evex_encodings_are_rejected() {
        // vzeroupper
        let (_, vex) = audit_encodings(&[0xc5, 0xf8, 0x77], 0x1000, 64);
        assert_eq!(vex.len(), 1);
        assert_eq!(vex[0].2, EncodingKind::VEX);
        // vmovaps zmm0, zmm1
        let (_, evex) = audit_encodings(&[0x62, 0xf1, 0x7c, 0x48, 0x28, 0xc1], 0x2000, 64);
        assert_eq!(evex.len(), 1);
        assert_eq!(evex[0].2, EncodingKind::EVEX);
    }

    #[test]
    fn legacy_vector_and_fxsave_encodings_are_allowed() {
        let (count, hits) = audit_encodings(
            &[
                0x0f, 0x28, 0xc1, // movaps
                0xf3, 0x0f, 0x6f, 0x01, // movdqu
                0x48, 0x0f, 0xae, 0x00, // fxsave64 [rax]
            ],
            0x3000,
            64,
        );
        assert_eq!(count, 3);
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn an_undecodable_instruction_is_rejected() {
        // `ud0` without its ModRM byte, truncated at the end of the code.
        let (_, hits) = audit_encodings(&[0x0f, 0xff], 0x4000, 64);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, Code::INVALID);
    }

    #[test]
    fn each_trampoline_mode_is_decoded_at_its_own_bitness() {
        // `mov eax, 1` in 16-bit code, which 64-bit decoding would misread.
        let (_, hits_16) = audit_encodings(&[0x66, 0xb8, 0x01, 0x00, 0x00, 0x00], 0x1000, 16);
        assert!(hits_16.is_empty());
        let (_, hits_32) = audit_encodings(&[0xc5, 0xf8, 0x77], 0x2000, 32);
        assert_eq!(hits_32.len(), 1);
        assert_eq!(hits_32[0].2, EncodingKind::VEX);
    }

    #[test]
    fn a_range_is_clipped_to_the_section() {
        let data = [0_u8, 1, 2, 3, 4, 5, 6, 7];
        let clipped = clip(&data, 0x1000, 0x1002..0x1005).expect("inside the section");
        assert_eq!((clipped.start, clipped.bytes), (0x1002, &data[2..5]));
        let tail = clip(&data, 0x1000, 0x1006..u64::MAX).expect("the section's tail");
        assert_eq!((tail.start, tail.bytes), (0x1006, &data[6..]));
        assert!(clip(&data, 0x1000, 0x0800..0x1000).is_none());
        assert!(clip(&data, 0x1000, 0x1008..0x2000).is_none());
    }
}
