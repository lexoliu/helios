//! The LZ4 block codec the compressed store will use, driven the way the
//! store drives it: one 4 KiB page in, a caller-owned output buffer, a
//! reused compress table, and a decode into a page-sized buffer that must
//! come out at exactly one page.

use helios_hal::pmm::PhysFrame;
use lz4_flex::block::{CompressTable, compress_into_with_table, decompress_into};

const PAGE: usize = PhysFrame::SIZE;

/// A real page of text: the first page of this module's neighbour
/// `space.rs`, whose source is far longer than two pages.
fn text_page() -> &'static [u8] {
    &include_bytes!("space.rs")[..PAGE]
}

#[test]
fn a_page_of_source_text_round_trips_and_shrinks() {
    let page = text_page();
    let mut table = CompressTable::default();
    let mut compressed = [0_u8; lz4_flex::block::get_maximum_output_size(PAGE)];
    let compressed_len =
        compress_into_with_table(page, &mut compressed, &mut table).expect("compress");
    assert!(
        compressed_len < PAGE * 3 / 4,
        "source text compresses to under three quarters of a page, got {compressed_len} bytes"
    );

    let mut restored = [0_u8; PAGE];
    let restored_len =
        decompress_into(&compressed[..compressed_len], &mut restored).expect("decompress");
    assert_eq!(restored_len, PAGE, "a page decodes to exactly one page");
    assert_eq!(&restored[..], page);

    // The table is reusable: a second page through the same table decodes
    // byte-exact too.
    let second = &include_bytes!("space.rs")[PAGE..2 * PAGE];
    let second_len = compress_into_with_table(second, &mut compressed, &mut table).expect("again");
    let mut again = [0_u8; PAGE];
    assert_eq!(
        decompress_into(&compressed[..second_len], &mut again).expect("decompress again"),
        PAGE
    );
    assert_eq!(&again[..], second);
}

#[test]
fn a_truncated_block_fails_to_decode_instead_of_yielding_a_page() {
    let page = text_page();
    let mut table = CompressTable::default();
    let mut compressed = [0_u8; lz4_flex::block::get_maximum_output_size(PAGE)];
    let compressed_len =
        compress_into_with_table(page, &mut compressed, &mut table).expect("compress");

    let mut restored = [0_u8; PAGE];
    let outcome = decompress_into(&compressed[..compressed_len / 2], &mut restored);
    assert!(
        !matches!(outcome, Ok(PAGE)),
        "half a block must not decode to a whole page"
    );
}
