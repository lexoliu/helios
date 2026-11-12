//! `policy.rs` ports the literal tables of `vn_protocol.py` and `vkxml.py`.
//! This test reads the pinned Python sources and fails when a table differs,
//! so moving the pin cannot silently leave the Rust tables behind.

mod support;

use helios_venus_gen::policy;

/// The text between `marker` and the first following `close` at nesting
/// depth zero, where `open` raises the depth.
fn block<'a>(text: &'a str, marker: &str, open: char, close: char) -> &'a str {
    let start = text
        .find(marker)
        .unwrap_or_else(|| panic!("`{marker}` is not in the pinned source"))
        + marker.len();
    let mut depth = 1;
    for (offset, c) in text[start..].char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return &text[start..start + offset];
            }
        }
    }
    panic!("`{marker}` is never closed");
}

/// Single-quoted strings, in order, outside comments.
fn strings(block: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in block.lines() {
        let code = line.split('#').next().unwrap_or("");
        let mut parts = code.split('\'');
        parts.next();
        while let (Some(inside), Some(_)) = (parts.next(), parts.next().or(Some(""))) {
            out.push(inside.to_owned());
        }
    }
    out
}

fn dictionary(block: &str) -> Vec<(String, u32)> {
    block
        .lines()
        .filter_map(|line| {
            let code = line.split('#').next()?.trim();
            let (key, value) = code.split_once(':')?;
            let key = key.trim().trim_matches('\'');
            let value = value.trim().trim_end_matches(',').parse().ok()?;
            Some((key.to_owned(), value))
        })
        .collect()
}

#[test]
fn tables_match_the_pinned_python() {
    let inputs = support::inputs();
    let protocol = inputs.read_to_string("vn_protocol.py").unwrap();
    let vkxml = inputs.read_to_string("vkxml.py").unwrap();

    let version = protocol
        .lines()
        .find_map(|line| line.strip_prefix("VN_WIRE_FORMAT_VERSION = "))
        .expect("the wire format version");
    assert_eq!(
        version.trim().parse::<u32>().unwrap(),
        policy::WIRE_FORMAT_VERSION
    );

    let extensions = strings(block(&protocol, "VK_XML_EXTENSION_LIST = [", '[', ']'));
    assert_eq!(extensions, policy::EXTENSION_LIST, "VK_XML_EXTENSION_LIST");

    let primitives = dictionary(block(&protocol, "PRIMITIVE_TYPES = {", '{', '}'));
    let ours: Vec<(String, u32)> = policy::PRIMITIVE_TYPES
        .iter()
        .map(|(name, size)| ((*name).to_owned(), *size))
        .collect();
    assert_eq!(primitives, ours, "PRIMITIVE_TYPES");

    let tags = dictionary(block(&protocol, "UNION_DEFAULT_TAGS = {", '{', '}'));
    let ours: Vec<(String, u32)> = policy::UNION_DEFAULT_TAGS
        .iter()
        .map(|(name, tag)| ((*name).to_owned(), *tag))
        .collect();
    assert_eq!(tags, ours, "UNION_DEFAULT_TAGS");

    let blocked = strings(block(&protocol, "COMMAND_BLOCK_LIST = [", '[', ']'));
    assert_eq!(blocked, policy::COMMAND_BLOCK_LIST, "COMMAND_BLOCK_LIST");

    let ignorable = strings(block(&protocol, "IGNORABLE_LIST = [", '[', ']'));
    let ours: Vec<String> = policy::IGNORABLE_LIST
        .iter()
        .flat_map(|item| {
            [
                item.struct_name.to_owned(),
                item.member.to_owned(),
                item.condition.c_text(),
            ]
        })
        .collect();
    assert_eq!(ignorable, ours, "IGNORABLE_LIST");

    let types = strings(block(&vkxml, "TYPE_BLOCK_LIST = [", '[', ']'));
    assert_eq!(types, policy::TYPE_BLOCK_LIST, "TYPE_BLOCK_LIST");
}

#[test]
fn a_drifted_table_is_caught() {
    // The comparison is not vacuous: a table with one entry changed differs.
    let text = "X = [\n    'a',\n    # 'skipped',\n    'b',\n]\n";
    assert_eq!(strings(block(text, "X = [", '[', ']')), ["a", "b"]);
    assert_ne!(strings(block(text, "X = [", '[', ']')), ["a", "c"]);
}
