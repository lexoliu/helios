//! Decompression of compressed extents.
//!
//! Each decoder writes into a caller buffer sized to the extent's
//! `ram_bytes`, which the item parser bounded by the format's 128 KiB, and
//! never allocates in proportion to anything the stream declares. A stream
//! that would produce more than `ram_bytes` is refused; one that produces
//! fewer leaves the rest zero, which is how the kernel reads the tail of a
//! file's last compressed extent.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::{DecompressorOxide, decompress as inflate, inflate_flags};
use zrip_core::error::DecompressError;
use zrip_core::frame::header::parse_frame_header;
use zrip_decode::context::DecompressContext;

use crate::error::{Compression, DecompressDefect};
use crate::ondisk::MAX_COMPRESSED_EXTENT;

/// Decompresses `src`, stored with `algorithm`, into `dst`.
pub(crate) fn decompress(
    algorithm: Compression,
    src: &[u8],
    dst: &mut [u8],
    sectorsize: usize,
) -> Result<(), DecompressDefect> {
    let produced = match algorithm {
        Compression::Zlib => zlib(src, dst)?,
        Compression::Lzo => lzo(src, dst, sectorsize)?,
        Compression::Zstd => zstd(src, dst)?,
    };
    dst[produced..].fill(0);
    Ok(())
}

/// One zlib stream per extent. The Adler-32 trailer is not checked, as the
/// kernel does not check it: the extent's data checksum already covers the
/// compressed bytes.
fn zlib(src: &[u8], dst: &mut [u8]) -> Result<usize, DecompressDefect> {
    // The decompressor state is about 11 KiB; it lives on the heap rather
    // than in the caller's stack frame.
    let mut state = Box::<DecompressorOxide>::default();
    let flags = inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER
        | inflate_flags::TINFL_FLAG_IGNORE_ADLER32
        | inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
    let (status, _, written) = inflate(&mut state, src, dst, 0, flags);
    match status {
        TINFLStatus::Done => Ok(written),
        TINFLStatus::HasMoreOutput => Err(DecompressDefect::Overrun(dst.len() as u64)),
        TINFLStatus::NeedsMoreInput | TINFLStatus::FailedCannotMakeProgress => {
            Err(DecompressDefect::Truncated)
        }
        _ => Err(DecompressDefect::Malformed),
    }
}

/// Bytes of an LZO length header.
const LZO_HEADER: usize = 4;

/// The btrfs LZO container: a little-endian total length, then segments
/// of a little-endian length and one LZO1X stream that decompresses to at
/// most one sector. A segment header never straddles a sector boundary of
/// the compressed bytes; the writer pads with zeroes to the next sector
/// instead.
fn lzo(src: &[u8], dst: &mut [u8], sectorsize: usize) -> Result<usize, DecompressDefect> {
    let read_len = |at: usize| -> Result<usize, DecompressDefect> {
        let bytes = src
            .get(at..at + LZO_HEADER)
            .ok_or(DecompressDefect::Truncated)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
    };
    let total = read_len(0)?;
    if total < LZO_HEADER || total > src.len() || total as u64 > MAX_COMPRESSED_EXTENT {
        return Err(DecompressDefect::Malformed);
    }
    let src = &src[..total];
    // The worst case LZO1X expansion of one sector of input.
    let max_segment = sectorsize + sectorsize / 16 + 64 + 3;
    let mut scratch = vec![0_u8; sectorsize];
    let mut cursor = LZO_HEADER;
    let mut produced = 0;
    while cursor < total {
        let segment_len = read_len(cursor)?;
        cursor += LZO_HEADER;
        if segment_len == 0 || segment_len > max_segment {
            return Err(DecompressDefect::Malformed);
        }
        let segment = src
            .get(cursor..cursor + segment_len)
            .ok_or(DecompressDefect::Truncated)?;
        cursor += segment_len;
        let written =
            lzokay::decompress::decompress(segment, &mut scratch).map_err(|error| match error {
                lzokay::Error::InputOverrun => DecompressDefect::Truncated,
                _ => DecompressDefect::Malformed,
            })?;
        let space = dst.len() - produced;
        if written > space {
            return Err(DecompressDefect::Overrun(dst.len() as u64));
        }
        dst[produced..produced + written].copy_from_slice(&scratch[..written]);
        produced += written;
        let sector_left = sectorsize - cursor % sectorsize;
        if sector_left < LZO_HEADER {
            cursor += sector_left;
        }
    }
    Ok(produced)
}

/// Bytes of the zstd frame magic number.
const ZSTD_MAGIC_BYTES: usize = 4;

/// One zstd frame per extent, padded with zeroes to the sector. The kernel
/// compresses with at most a 128 KiB window and no dictionary, so a frame
/// declaring either is refused. The decoder is held to `dst.len()` bytes
/// of output, which bounds every allocation it makes whatever the frame
/// claims, and it stops at the frame's end, ignoring the sector padding.
fn zstd(src: &[u8], dst: &mut [u8]) -> Result<usize, DecompressDefect> {
    let header = parse_frame_header(src).map_err(zstd_defect)?;
    if header.window_size > MAX_COMPRESSED_EXTENT {
        return Err(DecompressDefect::Window(header.window_size));
    }
    if header.dict_id.is_some() {
        return Err(DecompressDefect::Malformed);
    }
    let mut plain = Vec::with_capacity(dst.len());
    DecompressContext::new()
        .decompress_after_magic_into(&src[ZSTD_MAGIC_BYTES..], &mut plain, dst.len())
        .map_err(|error| match error {
            DecompressError::OutputTooSmall => DecompressDefect::Overrun(dst.len() as u64),
            other => zstd_defect(other),
        })?;
    dst[..plain.len()].copy_from_slice(&plain);
    Ok(plain.len())
}

fn zstd_defect(error: DecompressError) -> DecompressDefect {
    match error {
        DecompressError::InputExhausted => DecompressDefect::Truncated,
        DecompressError::WindowTooLarge { requested, .. } => DecompressDefect::Window(requested),
        _ => DecompressDefect::Malformed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTOR: usize = 4096;

    fn text(len: usize) -> Vec<u8> {
        b"btrfs compresses this line; \xe5\x9c\xa7\xe7\xb8\xae \xec\x95\x95\xec\xb6\x95 \xe5\x8e\x8b\xe7\xbc\xa9\n"
            .iter()
            .copied()
            .cycle()
            .take(len)
            .collect()
    }

    #[test]
    fn zlib_round_trips_and_zero_fills_the_tail() {
        let plain = text(10_000);
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
        let mut out = vec![0xff_u8; 12_288];
        decompress(Compression::Zlib, &packed, &mut out, SECTOR).unwrap();
        assert_eq!(&out[..plain.len()], &plain[..]);
        assert!(out[plain.len()..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn zlib_refuses_overrun_truncation_and_garbage() {
        let plain = text(10_000);
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
        let mut short = vec![0_u8; 8192];
        assert_eq!(
            decompress(Compression::Zlib, &packed, &mut short, SECTOR),
            Err(DecompressDefect::Overrun(8192))
        );
        let mut out = vec![0_u8; 12_288];
        assert_eq!(
            decompress(
                Compression::Zlib,
                &packed[..packed.len() / 2],
                &mut out,
                SECTOR
            ),
            Err(DecompressDefect::Truncated)
        );
        assert_eq!(
            decompress(
                Compression::Zlib,
                &[0x78, 0x9c, 0xff, 0xff, 0xff],
                &mut out,
                SECTOR
            ),
            Err(DecompressDefect::Malformed)
        );
    }

    /// Frames `plain` the way btrfs stores an LZO extent: one LZO1X stream
    /// per sector of input, a segment header never crossing a sector.
    fn lzo_extent(plain: &[u8]) -> Vec<u8> {
        let mut out = vec![0_u8; LZO_HEADER];
        for sector in plain.chunks(SECTOR) {
            let segment = lzokay::compress::compress(sector).unwrap();
            out.extend_from_slice(&(segment.len() as u32).to_le_bytes());
            out.extend_from_slice(&segment);
            let left = SECTOR - out.len() % SECTOR;
            if left < LZO_HEADER {
                out.resize(out.len() + left, 0);
            }
        }
        let total = out.len() as u32;
        out[..LZO_HEADER].copy_from_slice(&total.to_le_bytes());
        out
    }

    #[test]
    fn lzo_round_trips_across_sectors() {
        // Incompressible enough that segments cross sector boundaries.
        let plain: Vec<u8> = (0..40_000_u32)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let packed = lzo_extent(&plain);
        assert!(packed.len() > SECTOR);
        let mut out = vec![0_u8; 40_960];
        decompress(Compression::Lzo, &packed, &mut out, SECTOR).unwrap();
        assert_eq!(&out[..plain.len()], &plain[..]);
    }

    #[test]
    fn lzo_refuses_malformed_framing() {
        let packed = lzo_extent(&text(9000));
        let mut out = vec![0_u8; 12_288];
        let mut lying = packed.clone();
        lying[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decompress(Compression::Lzo, &lying, &mut out, SECTOR),
            Err(DecompressDefect::Malformed)
        );
        let mut short = vec![0_u8; 4096];
        assert_eq!(
            decompress(Compression::Lzo, &packed, &mut short, SECTOR),
            Err(DecompressDefect::Overrun(4096))
        );
        let mut cut = packed[..packed.len() - 10].to_vec();
        let total = cut.len() as u32;
        cut[..4].copy_from_slice(&total.to_le_bytes());
        assert!(decompress(Compression::Lzo, &cut, &mut out, SECTOR).is_err());
    }

    /// A zstd frame header with no checksum and a window of `2^log` bytes.
    fn zstd_header(window_log: u8) -> Vec<u8> {
        vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, (window_log - 10) << 3]
    }

    fn zstd_block(last: bool, kind: u32, size: u32) -> [u8; 3] {
        let header = (size << 3) | (kind << 1) | u32::from(last);
        let bytes = header.to_le_bytes();
        [bytes[0], bytes[1], bytes[2]]
    }

    #[test]
    fn zstd_decodes_raw_and_rle_blocks() {
        let mut frame = zstd_header(17);
        frame.extend_from_slice(&zstd_block(false, 0, 3));
        frame.extend_from_slice(b"abc");
        frame.extend_from_slice(&zstd_block(true, 1, 5));
        frame.push(b'z');
        frame.resize(SECTOR, 0);
        let mut out = vec![0xff_u8; 16];
        decompress(Compression::Zstd, &frame, &mut out, SECTOR).unwrap();
        assert_eq!(&out, b"abczzzzz\0\0\0\0\0\0\0\0");
    }

    #[test]
    fn zstd_refuses_a_large_window_and_an_expansion_past_the_extent() {
        let mut frame = zstd_header(21);
        frame.extend_from_slice(&zstd_block(true, 0, 3));
        frame.extend_from_slice(b"abc");
        let mut out = vec![0_u8; 16];
        assert_eq!(
            decompress(Compression::Zstd, &frame, &mut out, SECTOR),
            Err(DecompressDefect::Window(2 << 20))
        );

        // A few hundred bytes claiming 100 blocks of 128 KiB each: decoded
        // a block at a time, it stops at the extent's size.
        let mut bomb = zstd_header(17);
        for index in 0..100 {
            bomb.extend_from_slice(&zstd_block(index == 99, 1, 128 << 10));
            bomb.push(b'x');
        }
        let mut out = vec![0_u8; 128 << 10];
        assert_eq!(
            decompress(Compression::Zstd, &bomb, &mut out, SECTOR),
            Err(DecompressDefect::Overrun(128 << 10))
        );
    }
}
