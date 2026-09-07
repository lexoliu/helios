//! The byte-level plumbing every rewrite in this crate shares.
//!
//! Both rewrites — instrumenting a module and hinting one — work by
//! splicing bytes rather than by re-encoding, so a section this tool has no
//! opinion about survives byte for byte and the offsets a profile records
//! stay meaningful.

use std::ops::Range;

use wasm_encoder::Encode;

/// Encodes `value` as an unsigned LEB128 integer.
pub fn leb_u32(value: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    value.encode(&mut bytes);
    bytes
}

/// Locates the header byte of the section whose content is `content`: the
/// id byte, followed by the LEB128 size, immediately precedes the content.
/// The id found there has to be `id`, or the section was not where its
/// length said it was.
pub fn header_start(bytes: &[u8], id: u8, content: &Range<usize>) -> usize {
    let size = u32::try_from(content.len()).expect("a wasm section length fits in a u32");
    let start = content.start - 1 - leb_u32(size).len();
    assert_eq!(
        bytes[start], id,
        "section header for id {id} is not where its length says it is"
    );
    start
}

/// Decodes the unsigned LEB128 integer at `bytes[position]`, returning it
/// and the offset just past it.
pub fn read_leb_u32(bytes: &[u8], position: usize) -> Option<(u32, usize)> {
    let mut result: u32 = 0;
    let mut shift = 0;
    let mut cursor = position;
    loop {
        let byte = *bytes.get(cursor)?;
        cursor += 1;
        result |= u32::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some((result, cursor));
        }
        shift += 7;
        if shift > 31 {
            return None;
        }
    }
}

/// Wraps `content` in a section header for `id`.
pub fn section(id: u8, content: &[u8]) -> Vec<u8> {
    let mut bytes = vec![id];
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a wasm section length is a u32 by construction"
    )]
    let len = content.len() as u32;
    bytes.extend(leb_u32(len));
    bytes.extend_from_slice(content);
    bytes
}

/// Rebuilds a vector-shaped section's content with `extra` entries
/// appended, leaving the existing entries' bytes untouched.
///
/// A vector section is a LEB count followed by the entries; appending is
/// therefore a new count in front of the original entry bytes. Re-encoding
/// the entries instead would mean understanding every type form in the
/// section, which this tool has no reason to.
pub fn append_to_vector_section(content: &[u8], count: u32, added: u32, extra: &[u8]) -> Vec<u8> {
    let entries_start = read_leb_u32(content, 0)
        .expect("a vector section this crate reached has a decodable count")
        .1;
    let mut bytes = leb_u32(count + added);
    bytes.extend_from_slice(&content[entries_start..]);
    bytes.extend_from_slice(extra);
    bytes
}

/// A `(range, replacement)` edit against one function body.
#[derive(Debug, Clone)]
pub struct Splice {
    pub range: std::ops::Range<usize>,
    pub bytes: Vec<u8>,
}

/// Applies `splices` to `bytes`. The ranges are relative to `bytes`, must
/// be disjoint, and are applied in ascending order.
pub fn apply_splices(bytes: &[u8], mut splices: Vec<Splice>) -> Vec<u8> {
    splices.sort_by_key(|splice| splice.range.start);
    let mut out = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    for splice in splices {
        assert!(
            splice.range.start >= cursor,
            "splices must be disjoint and ascending"
        );
        out.extend_from_slice(&bytes[cursor..splice.range.start]);
        out.extend_from_slice(&splice.bytes);
        cursor = splice.range.end;
    }
    out.extend_from_slice(&bytes[cursor..]);
    out
}

/// Encodes a function type entry as it appears in the type section.
pub fn func_type_entry(
    params: &[wasm_encoder::ValType],
    results: &[wasm_encoder::ValType],
) -> Vec<u8> {
    let mut bytes = vec![0x60];
    #[expect(
        clippy::cast_possible_truncation,
        reason = "these are the handful of types this crate appends"
    )]
    let param_count = params.len() as u32;
    bytes.extend(leb_u32(param_count));
    for param in params {
        param.encode(&mut bytes);
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "these are the handful of types this crate appends"
    )]
    let result_count = results.len() as u32;
    bytes.extend(leb_u32(result_count));
    for result in results {
        result.encode(&mut bytes);
    }
    bytes
}

/// Prefixes a function body with its LEB length, the shape the code
/// section stores.
pub fn sized_body(body: &[u8]) -> Vec<u8> {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a wasm function body length is a u32 by construction"
    )]
    let len = body.len() as u32;
    let mut bytes = leb_u32(len);
    bytes.extend_from_slice(body);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb_round_trips() {
        for value in [0u32, 1, 127, 128, 624_485, u32::MAX] {
            let bytes = leb_u32(value);
            assert_eq!(read_leb_u32(&bytes, 0), Some((value, bytes.len())));
        }
    }

    #[test]
    fn splices_replace_disjoint_ranges() {
        let spliced = apply_splices(
            b"abcdef",
            vec![
                Splice {
                    range: 1..2,
                    bytes: b"XY".to_vec(),
                },
                Splice {
                    range: 4..4,
                    bytes: b"-".to_vec(),
                },
            ],
        );
        assert_eq!(spliced, b"aXYcd-ef");
    }

    #[test]
    fn appending_keeps_existing_entries() {
        // A two-entry vector section: count, then the entries.
        let content = [0x02, 0xaa, 0xbb];
        let appended = append_to_vector_section(&content, 2, 1, &[0xcc]);
        assert_eq!(appended, [0x03, 0xaa, 0xbb, 0xcc]);
    }
}
