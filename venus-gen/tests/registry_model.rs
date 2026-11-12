//! Facts about the registry model that the memory layout and the wire order
//! each depend on.

mod support;

use helios_venus_gen::analysis::Analysis;
use helios_venus_gen::layout::Layouts;
use helios_venus_gen::registry::{Category, Registry};

fn analysis() -> Analysis {
    let inputs = support::inputs();
    let registry = Registry::parse(
        &inputs.path("xmls/vk.xml"),
        &[
            &inputs.path("xmls/VK_EXT_command_serialization.xml"),
            &inputs.path("xmls/VK_MESA_venus_protocol.xml"),
        ],
    )
    .expect("the registry parses");
    Analysis::new(registry).expect("the registry is analysed")
}

/// A length member moves before its array on the wire; the C declaration
/// keeps its own order, and the memory layout follows the declaration.
#[test]
fn the_wire_order_and_the_declaration_order_are_kept_apart() {
    let an = analysis();
    let layouts = Layouts::new(&an.reg, 8);
    let mut reordered = Vec::new();
    for (id, ty) in an.reg.types.iter().enumerate() {
        if ty.category != Some(Category::Struct) || ty.variables.is_empty() {
            continue;
        }
        let identity: Vec<usize> = (0..ty.variables.len()).collect();
        let mut sorted = ty.declared_order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, identity, "{} keeps every member once", ty.name);
        if ty.declared_order != identity {
            reordered.push(id);
        }
        if let Ok(layout) = layouts.of_aggregate(id) {
            // Offsets grow in declaration order.
            let offsets: Vec<u64> = ty.declared().map(|(i, _)| layout.offsets[i]).collect();
            assert!(
                offsets.windows(2).all(|w| w[0] <= w[1]),
                "{} places its members in declaration order",
                ty.name
            );
        }
    }
    assert!(
        !reordered.is_empty(),
        "the registry has structs whose length member is declared after the array"
    );
}

#[test]
fn enumerators_defined_by_offset_have_the_registry_values() {
    let an = analysis();
    let structure_type = an.reg.require("VkStructureType").expect("VkStructureType");
    let result = an.reg.require("VkResult").expect("VkResult");
    // `offset` 3 of extension 471, positive.
    assert_eq!(
        an.reg
            .enum_value(structure_type, "VK_STRUCTURE_TYPE_RENDERING_AREA_INFO")
            .unwrap(),
        1_000_470_003
    );
    // `offset` 5 of extension 70 with `dir="-"`.
    assert_eq!(
        an.reg
            .enum_value(result, "VK_ERROR_OUT_OF_POOL_MEMORY")
            .unwrap(),
        -1_000_069_000
    );
}
