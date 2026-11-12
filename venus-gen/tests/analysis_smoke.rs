//! The pinned registry loads into the model and the analysis accepts it.

mod support;

use helios_venus_gen::analysis::Analysis;
use helios_venus_gen::registry::Registry;

#[test]
fn registry_and_analysis_load() {
    let inputs = support::inputs();
    let vk = inputs.path("xmls/vk.xml");
    let serialization = inputs.path("xmls/VK_EXT_command_serialization.xml");
    let venus = inputs.path("xmls/VK_MESA_venus_protocol.xml");
    let registry =
        Registry::parse(&vk, &[&serialization, &venus]).unwrap_or_else(|e| panic!("{e}"));
    let analysis = Analysis::new(registry).unwrap_or_else(|e| panic!("{e}"));
    eprintln!(
        "commands {} (skipped {}), structs {} (skipped {})",
        analysis.generated_commands.len(),
        analysis.skipped_commands.len(),
        analysis.generated_structs.len(),
        analysis.skipped_structs.len()
    );
    assert!(!analysis.generated_commands.is_empty());
}
