//! The LLVM raw profile (`.profraw`) container, written from the sections
//! the linker laid out.
//!
//! The format is the one `llvm-profdata` reads: a 16-field header followed by
//! the `__llvm_prf_data`, `__llvm_prf_cnts` and `__llvm_prf_names` bytes
//! verbatim, each group padded to eight bytes. Nothing in a record needs
//! rewriting on the way out — since raw version 8 a record's counter pointer
//! is a link-time relative offset, and the header's `CountersDelta` is what
//! turns it back into an index — so the image is a concatenation and can be
//! served a window at a time without a buffer to build it in.

use core::marker::PhantomData;

use super::{LlvmProfileError, ProfileSection};

/// Raw-profile format version this writer emits.
///
/// The number is not a guess. Every object rustc instruments carries
/// `__llvm_profile_raw_version`, the version word LLVM's own runtime would
/// have written, and [`RawProfile::new`] refuses to write a byte until that
/// word matches this constant. On the pinned toolchain (`rust-toolchain.toml`,
/// `nightly-2026-06-15`, `rustc -vV` reports LLVM 22.1.6) the symbol reads
/// `0x0100_0000_0000_000a`: version 10, with the IR-instrumentation variant
/// bit set. Ten is `INSTR_PROF_RAW_VERSION` in LLVM's
/// `llvm/include/llvm/ProfileData/InstrProfData.inc`, and it is what fixes
/// the header below at sixteen fields.
pub const RAW_PROFILE_VERSION: u64 = 10;

/// `VARIANT_MASK_IR_PROF`: the profile came from IR-level instrumentation,
/// which is what `-C profile-generate` asks rustc for.
const VARIANT_MASK_IR_PROF: u64 = 1 << 56;

/// `INSTR_PROF_RAW_MAGIC_64`, the first eight bytes of a 64-bit raw profile.
const MAGIC: u64 = (255 << 56)
    | ((b'l' as u64) << 48)
    | ((b'p' as u64) << 40)
    | ((b'r' as u64) << 32)
    | ((b'o' as u64) << 24)
    | ((b'f' as u64) << 16)
    | ((b'r' as u64) << 8)
    | 129;

/// `IPVK_Last`: the highest value-profiling kind the format knows about
/// (indirect-call target, memop size, vtable target). The kernel's
/// instrumentation is built with value profiling off, so no record carries
/// value sites, but the header field names the format's own limit.
const VALUE_KIND_LAST: u64 = 2;

/// Size of one `__llvm_profile_data` record on a 64-bit target: six pointer
/// or 64-bit fields, three 32-bit fields, padded to eight.
const DATA_RECORD_LEN: u64 = 64;

/// Size of one profile counter.
const COUNTER_LEN: u64 = 8;

/// Number of `u64` fields in the raw-profile header, in the order
/// [`RawProfile::header`] writes them.
const HEADER_FIELDS: usize = 16;

/// Byte length of the raw-profile header.
const HEADER_LEN: u64 = HEADER_FIELDS as u64 * 8;

const _: () = assert!(
    size_of::<usize>() == 8,
    "the raw profile's record and pointer widths are the 64-bit ones; Helios targets are 64-bit"
);

/// A `__llvm_prf_*` section, as the linker bounded it.
///
/// The span is held as a pointer and a length rather than a slice: the
/// counter section is written by instrumented code on every processor while
/// the profile is read, so no shared reference to it is ever created.
#[derive(Clone, Copy)]
pub struct SectionSpan<'a> {
    start: *const u8,
    len: u64,
    marker: PhantomData<&'a [u8]>,
}

impl<'a> SectionSpan<'a> {
    /// Builds a span over `len` bytes at `start`.
    ///
    /// # Safety
    ///
    /// `start` must point at `len` readable bytes that stay mapped for `'a`.
    /// The bytes may be written concurrently — that is what a live counter
    /// section does — but they must never be unmapped or reallocated.
    pub const unsafe fn new(start: *const u8, len: u64) -> Self {
        Self {
            start,
            len,
            marker: PhantomData,
        }
    }

    /// Builds a span over bytes the caller owns.
    #[cfg(test)]
    pub fn from_slice(bytes: &'a [u8]) -> Self {
        // SAFETY: the slice's bytes are readable for `'a` by construction.
        unsafe { Self::new(bytes.as_ptr(), bytes.len() as u64) }
    }

    /// Runtime address of the first byte, which the header records so the
    /// reader can turn a record's relative counter pointer into an index.
    fn addr(self) -> u64 {
        self.start.addr() as u64
    }

    fn len(self) -> u64 {
        self.len
    }

    /// Copies `out.len()` bytes starting `within` bytes into the section.
    fn copy_into(self, within: u64, out: &mut [u8]) {
        debug_assert!(within + out.len() as u64 <= self.len);
        // SAFETY: `new`'s contract makes `start .. start + len` readable, and
        // the debug assertion above holds for every window `read_at` builds.
        // A counter the instrumentation increments during the copy yields
        // either its old or its new value, which is what a profile written
        // from a running program always records.
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.start.add(within as usize),
                out.as_mut_ptr(),
                out.len(),
            );
        }
    }
}

/// One run of bytes in the serialised image.
#[derive(Clone, Copy)]
enum Segment<'a> {
    /// The header, built on demand from the section bounds.
    Header,
    /// A `__llvm_prf_*` section, copied verbatim.
    Section(SectionSpan<'a>),
    /// Zero padding that aligns the next group to eight bytes.
    Padding(u64),
}

impl Segment<'_> {
    fn len(self) -> u64 {
        match self {
            Self::Header => HEADER_LEN,
            Self::Section(span) => span.len(),
            Self::Padding(len) => len,
        }
    }

    fn copy_into(self, header: &[u8; HEADER_LEN as usize], within: u64, out: &mut [u8]) {
        match self {
            Self::Header => {
                let start = within as usize;
                out.copy_from_slice(&header[start..start + out.len()]);
            }
            Self::Section(span) => span.copy_into(within, out),
            Self::Padding(_) => out.fill(0),
        }
    }
}

/// The `.profraw` image of one instrumented kernel.
pub struct RawProfile<'a> {
    data: SectionSpan<'a>,
    counters: SectionSpan<'a>,
    names: SectionSpan<'a>,
    version: u64,
}

impl<'a> RawProfile<'a> {
    /// Describes the image the three sections make up.
    ///
    /// `version` is the instrumented image's own `__llvm_profile_raw_version`
    /// word: a version this writer does not implement, or a variant that
    /// would add a section to the file, is refused here rather than written
    /// out as a file `llvm-profdata` would misread.
    pub fn new(
        data: SectionSpan<'a>,
        counters: SectionSpan<'a>,
        names: SectionSpan<'a>,
        version: u64,
    ) -> Result<Self, LlvmProfileError> {
        if version != RAW_PROFILE_VERSION | VARIANT_MASK_IR_PROF {
            return Err(LlvmProfileError::UnsupportedVersion {
                found: version,
                implemented: RAW_PROFILE_VERSION,
            });
        }
        if !data.len().is_multiple_of(DATA_RECORD_LEN) {
            return Err(LlvmProfileError::MalformedSection {
                section: ProfileSection::Data,
                len: data.len(),
                record_len: DATA_RECORD_LEN,
            });
        }
        if !counters.len().is_multiple_of(COUNTER_LEN) {
            return Err(LlvmProfileError::MalformedSection {
                section: ProfileSection::Counters,
                len: counters.len(),
                record_len: COUNTER_LEN,
            });
        }
        Ok(Self {
            data,
            counters,
            names,
            version,
        })
    }

    /// Total length of the serialised image.
    pub fn len(&self) -> u64 {
        self.segments().iter().map(|segment| segment.len()).sum()
    }

    /// Copies the window at `offset` into `out`, returning how much it wrote.
    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, LlvmProfileError> {
        let len = self.len();
        if offset > len {
            return Err(LlvmProfileError::OutOfRange { offset, len });
        }
        let header = self.header();
        let mut written = 0usize;
        let mut segment_start = 0u64;
        for segment in self.segments() {
            let segment_end = segment_start + segment.len();
            let position = offset + written as u64;
            if written == out.len() {
                break;
            }
            if position >= segment_end {
                segment_start = segment_end;
                continue;
            }
            let within = position - segment_start;
            let available = segment.len() - within;
            let take = available.min((out.len() - written) as u64) as usize;
            segment.copy_into(&header, within, &mut out[written..written + take]);
            written += take;
            segment_start = segment_end;
        }
        Ok(written)
    }

    /// The image's segments, in the order compiler-rt's writer emits them.
    fn segments(&self) -> [Segment<'a>; 7] {
        [
            Segment::Header,
            Segment::Section(self.data),
            Segment::Padding(padding(self.data.len())),
            Segment::Section(self.counters),
            Segment::Padding(padding(self.counters.len())),
            Segment::Section(self.names),
            Segment::Padding(padding(self.names.len())),
        ]
    }

    /// Builds the header.
    ///
    /// The bitmap fields are zero because the kernel is instrumented without
    /// MC/DC coverage, and the vtable fields because it is instrumented
    /// without value profiling; a reader consults neither delta while every
    /// record reports no bitmap bytes and no value sites.
    fn header(&self) -> [u8; HEADER_LEN as usize] {
        let fields: [u64; HEADER_FIELDS] = [
            MAGIC,
            self.version,
            // BinaryIdsSize: the kernel image carries no build id in the
            // profile, so a merge names the image by its counters alone.
            0,
            self.data.len() / DATA_RECORD_LEN,
            padding(self.data.len()),
            self.counters.len() / COUNTER_LEN,
            padding(self.counters.len()),
            // NumBitmapBytes, PaddingBytesAfterBitmapBytes.
            0,
            0,
            self.names.len(),
            // CountersDelta and BitmapDelta are measured from the data
            // section, and the reader walks both down one record at a time.
            self.counters.addr().wrapping_sub(self.data.addr()),
            0,
            self.names.addr(),
            // NumVTables, VNamesSize.
            0,
            0,
            VALUE_KIND_LAST,
        ];
        let mut header = [0u8; HEADER_LEN as usize];
        for (slot, field) in header.chunks_exact_mut(8).zip(fields) {
            slot.copy_from_slice(&field.to_le_bytes());
        }
        header
    }
}

/// Zero bytes that follow a group of `size` bytes to align the next one.
///
/// This is `__llvm_profile_get_num_padding_bytes` from compiler-rt, and the
/// header records each count so the reader can skip exactly as much.
const fn padding(size: u64) -> u64 {
    7 & (8 - size % 8)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::*;

    const VERSION_WORD: u64 = RAW_PROFILE_VERSION | VARIANT_MASK_IR_PROF;

    fn sections() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        // Two data records, three counters, and a name blob whose length is
        // deliberately not a multiple of eight so the trailing padding is
        // exercised.
        let data = (0..2 * DATA_RECORD_LEN as u8).collect::<Vec<_>>();
        let counters = (0..3 * COUNTER_LEN as u8).map(|byte| byte ^ 0xa5).collect();
        let names = (0..21u8).map(|byte| byte | 0x40).collect();
        (data, counters, names)
    }

    fn profile<'a>(data: &'a [u8], counters: &'a [u8], names: &'a [u8]) -> RawProfile<'a> {
        RawProfile::new(
            SectionSpan::from_slice(data),
            SectionSpan::from_slice(counters),
            SectionSpan::from_slice(names),
            VERSION_WORD,
        )
        .expect("the fixture sections are whole records of a version this writer implements")
    }

    fn serialise(profile: &RawProfile<'_>) -> Vec<u8> {
        let mut bytes = vec![0u8; profile.len() as usize];
        let written = profile
            .read_at(0, &mut bytes)
            .expect("offset zero is inside the image");
        assert_eq!(written, bytes.len());
        bytes
    }

    fn field(bytes: &[u8], index: usize) -> u64 {
        u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().expect("8 bytes"))
    }

    #[test]
    fn header_describes_the_sections() {
        let (data, counters, names) = sections();
        let profile = profile(&data, &counters, &names);
        let bytes = serialise(&profile);

        assert_eq!(field(&bytes, 0), MAGIC);
        assert_eq!(field(&bytes, 1), VERSION_WORD);
        assert_eq!(field(&bytes, 2), 0, "binary ids");
        assert_eq!(field(&bytes, 3), 2, "data records");
        assert_eq!(field(&bytes, 4), 0, "padding before counters");
        assert_eq!(field(&bytes, 5), 3, "counters");
        assert_eq!(field(&bytes, 6), 0, "padding after counters");
        assert_eq!(field(&bytes, 7), 0, "bitmap bytes");
        assert_eq!(field(&bytes, 9), names.len() as u64);
        assert_eq!(
            field(&bytes, 10),
            (counters.as_ptr().addr() as u64).wrapping_sub(data.as_ptr().addr() as u64),
            "counters delta"
        );
        assert_eq!(field(&bytes, 12), names.as_ptr().addr() as u64);
        assert_eq!(field(&bytes, 15), VALUE_KIND_LAST);
    }

    #[test]
    fn payload_is_the_sections_padded_to_eight() {
        let (data, counters, names) = sections();
        let profile = profile(&data, &counters, &names);
        let bytes = serialise(&profile);

        let mut expected = Vec::new();
        expected.extend_from_slice(&data);
        expected.extend_from_slice(&counters);
        expected.extend_from_slice(&names);
        expected.extend_from_slice(&[0, 0, 0]);
        assert_eq!(&bytes[HEADER_LEN as usize..], expected.as_slice());
        assert_eq!(bytes.len() as u64 % 8, 0);
    }

    #[test]
    fn windows_of_every_size_rebuild_the_image() {
        let (data, counters, names) = sections();
        let profile = profile(&data, &counters, &names);
        let whole = serialise(&profile);

        for window in [1usize, 7, 8, 63, 128, 129, whole.len()] {
            let mut rebuilt = Vec::new();
            let mut buffer = vec![0u8; window];
            loop {
                let written = profile
                    .read_at(rebuilt.len() as u64, &mut buffer)
                    .expect("every offset walked is inside the image");
                if written == 0 {
                    break;
                }
                rebuilt.extend_from_slice(&buffer[..written]);
            }
            assert_eq!(rebuilt, whole, "window of {window} bytes");
        }
    }

    #[test]
    fn reading_at_the_end_writes_nothing_and_past_it_is_an_error() {
        let (data, counters, names) = sections();
        let profile = profile(&data, &counters, &names);
        let len = profile.len();

        let mut buffer = [0u8; 8];
        assert_eq!(
            profile.read_at(len, &mut buffer),
            Ok(0),
            "the end of the image is a legal offset"
        );
        assert_eq!(
            profile.read_at(len + 1, &mut buffer),
            Err(LlvmProfileError::OutOfRange {
                offset: len + 1,
                len
            })
        );
    }

    #[test]
    fn a_version_the_writer_does_not_implement_is_refused() {
        let (data, counters, names) = sections();
        let found = VERSION_WORD + 1;
        assert_eq!(
            RawProfile::new(
                SectionSpan::from_slice(&data),
                SectionSpan::from_slice(&counters),
                SectionSpan::from_slice(&names),
                found,
            )
            .err(),
            Some(LlvmProfileError::UnsupportedVersion {
                found,
                implemented: RAW_PROFILE_VERSION,
            })
        );
    }

    #[test]
    fn a_section_that_is_not_whole_records_is_refused() {
        let (data, counters, names) = sections();
        assert_eq!(
            RawProfile::new(
                SectionSpan::from_slice(&data[..data.len() - 1]),
                SectionSpan::from_slice(&counters),
                SectionSpan::from_slice(&names),
                VERSION_WORD,
            )
            .err(),
            Some(LlvmProfileError::MalformedSection {
                section: ProfileSection::Data,
                len: data.len() as u64 - 1,
                record_len: DATA_RECORD_LEN,
            })
        );
    }
}
