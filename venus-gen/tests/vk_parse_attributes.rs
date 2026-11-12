//! `vk-parse` is the registry parser the generator is built on. Before the
//! generator relies on it, this test establishes that it loads the pinned
//! `vk.xml` and the two private XMLs without losing the attributes the Venus
//! policy reads (`selector`, `selection`, `stride`, `altlen`, `len`, `api`,
//! `optional`, `noautovalidity`, `values`, `structextends`, `depends`), by
//! counting each attribute in the raw XML text and in the parsed model.

mod support;

use std::collections::BTreeMap;

use vk_parse::{
    Command, EnumSpec, EnumsChild, ExtensionChild, InterfaceItem, Registry, RegistryChild,
    TypeMember, TypeSpec, TypesChild,
};

/// Occurrences of ` name="` inside elements named `tags` in raw XML.
fn raw_attribute_count(xml: &str, tags: &[&str], attribute: &str) -> usize {
    let needle = format!(" {attribute}=\"");
    let mut count = 0;
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let end = rest.find('>').expect("unterminated tag");
        let tag = &rest[..end];
        let name: String = tag
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if tags.contains(&name.as_str()) {
            count += tag.matches(&needle).count();
        }
    }
    count
}

#[derive(Default)]
struct Parsed(BTreeMap<&'static str, usize>);

impl Parsed {
    fn bump(&mut self, key: &'static str, present: bool) {
        if present {
            *self.0.entry(key).or_default() += 1;
        }
    }
    fn get(&self, key: &str) -> usize {
        self.0.get(key).copied().unwrap_or(0)
    }
}

fn parse(path: &std::path::Path) -> Registry {
    let (registry, errors) = vk_parse::parse_file(path).expect("vk-parse parses the file");
    assert!(errors.is_empty(), "{path:?}: parse errors {errors:#?}");
    registry
}

fn tally(registry: &Registry) -> Parsed {
    let mut parsed = Parsed::default();
    for child in &registry.0 {
        match child {
            RegistryChild::Types(types) => {
                for child in &types.children {
                    let TypesChild::Type(ty) = child else {
                        continue;
                    };
                    parsed.bump("type.api", ty.api.is_some());
                    parsed.bump("type.structextends", ty.structextends.is_some());
                    let TypeSpec::Members(members) = &ty.spec else {
                        continue;
                    };
                    for member in members {
                        let TypeMember::Definition(member) = member else {
                            continue;
                        };
                        parsed.bump("member.len", member.len.is_some());
                        parsed.bump("member.altlen", member.altlen.is_some());
                        parsed.bump("member.optional", member.optional.is_some());
                        parsed.bump("member.selector", member.selector.is_some());
                        parsed.bump("member.selection", member.selection.is_some());
                        parsed.bump("member.noautovalidity", member.noautovalidity.is_some());
                        parsed.bump("member.values", member.values.is_some());
                        parsed.bump("member.api", member.api.is_some());
                    }
                }
            }
            RegistryChild::Commands(commands) => {
                for command in &commands.children {
                    let Command::Definition(command) = command else {
                        continue;
                    };
                    parsed.bump("command.api", command.api.is_some());
                    for param in &command.params {
                        parsed.bump("param.len", param.len.is_some());
                        parsed.bump("param.altlen", param.altlen.is_some());
                        parsed.bump("param.optional", param.optional.is_some());
                        parsed.bump("param.stride", param.stride.is_some());
                        parsed.bump("param.noautovalidity", param.noautovalidity.is_some());
                        parsed.bump("param.api", param.api.is_some());
                    }
                }
            }
            RegistryChild::Extensions(extensions) => {
                for extension in &extensions.children {
                    parsed.bump("extension.promotedto", extension.promotedto.is_some());
                    parsed.bump("extension.supported", extension.supported.is_some());
                    for child in &extension.children {
                        if let ExtensionChild::Require { depends, items, .. } = child {
                            parsed.bump("require.depends", depends.is_some());
                            for item in items {
                                if let InterfaceItem::Enum(item) = item {
                                    parsed.bump("enum.api", item.api.is_some());
                                    parsed.bump(
                                        "enum.offset",
                                        matches!(item.spec, EnumSpec::Offset { .. }),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            RegistryChild::Enums(enums) => {
                parsed.bump("enums.bitwidth", enums.bitwidth.is_some());
                for child in &enums.children {
                    if let EnumsChild::Enum(item) = child {
                        parsed.bump("enum.api", item.api.is_some());
                    }
                }
            }
            _ => {}
        }
    }
    parsed
}

#[test]
fn vk_parse_keeps_every_attribute_the_generator_reads() {
    let inputs = support::inputs();
    let mut checked = 0;
    for file in [
        "xmls/vk.xml",
        "xmls/VK_EXT_command_serialization.xml",
        "xmls/VK_MESA_venus_protocol.xml",
    ] {
        let xml = inputs.read_to_string(file).unwrap();
        let parsed = tally(&parse(&inputs.path(file)));
        // (raw element names, raw attribute, parsed key). Members and params
        // are separate keys in the model, so the raw count of an attribute on
        // `member` is compared with the parsed `member.*` count.
        let table: &[(&[&str], &str, &str)] = &[
            (&["member"], "len", "member.len"),
            (&["member"], "altlen", "member.altlen"),
            (&["member"], "optional", "member.optional"),
            (&["member"], "selector", "member.selector"),
            (&["member"], "selection", "member.selection"),
            (&["member"], "noautovalidity", "member.noautovalidity"),
            (&["member"], "values", "member.values"),
            (&["param"], "len", "param.len"),
            (&["param"], "altlen", "param.altlen"),
            (&["param"], "optional", "param.optional"),
            (&["param"], "stride", "param.stride"),
            (&["param"], "noautovalidity", "param.noautovalidity"),
            (&["type"], "structextends", "type.structextends"),
            (&["extension"], "promotedto", "extension.promotedto"),
            (&["extension"], "supported", "extension.supported"),
        ];
        for (tags, attribute, key) in table {
            let raw = raw_attribute_count(&xml, tags, attribute);
            let kept = parsed.get(key);
            assert_eq!(
                raw, kept,
                "{file}: {attribute} on <{}> occurs {raw} times in the XML but \
                 {kept} survive in vk-parse's model",
                tags[0]
            );
            checked += raw;
        }
    }
    assert!(
        checked > 1000,
        "the table checked only {checked} attributes"
    );
}

#[test]
fn private_xmls_declare_the_extensions_the_policy_names() {
    let inputs = support::inputs();
    for (file, extension) in [
        (
            "xmls/VK_EXT_command_serialization.xml",
            "VK_EXT_command_serialization",
        ),
        ("xmls/VK_MESA_venus_protocol.xml", "VK_MESA_venus_protocol"),
    ] {
        let registry = parse(&inputs.path(file));
        let found = registry.0.iter().any(|child| match child {
            RegistryChild::Extensions(extensions) => {
                extensions.children.iter().any(|ext| ext.name == extension)
            }
            _ => false,
        });
        assert!(
            found,
            "{file} does not declare {extension} in vk-parse's model"
        );
    }
}
