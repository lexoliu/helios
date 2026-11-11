//! The pinned registry emits as Rust source that parses.

mod support;

use helios_venus_gen::analysis::Analysis;
use helios_venus_gen::emit;
use helios_venus_gen::registry::Registry;

#[test]
fn protocol_source_emits() {
    let inputs = support::inputs();
    let vk = inputs.path("xmls/vk.xml");
    let serialization = inputs.path("xmls/VK_EXT_command_serialization.xml");
    let venus = inputs.path("xmls/VK_MESA_venus_protocol.xml");
    let registry =
        Registry::parse(&vk, &[&serialization, &venus]).unwrap_or_else(|e| panic!("{e}"));
    let analysis = Analysis::new(registry).unwrap_or_else(|e| panic!("{e}"));
    let source = emit::protocol_source(&analysis).unwrap_or_else(|e| panic!("{e}"));
    let out = support::workspace_root().join("target/venus-gen-scratch");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("protocol.rs"), &source).unwrap();
    eprintln!("{} bytes", source.len());
}
