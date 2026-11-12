//! The layout model behind the generated layout assertions, checked by a C
//! compiler.
//!
//! The generated Rust types carry `size_of`, `align_of` and `offset_of`
//! assertions for 4- and 8-byte pointers, computed by `venus-gen`'s layout
//! model; they are evaluated for whichever width the crate is built for
//! (`wasm32-wasip2` builds check the 4-byte ones). This test closes the loop
//! on every host: it renders the same numbers as C `_Static_assert`s over the
//! pinned `vulkan.h` and has `clang` check them for a wasm32 target and for
//! the host, so the model equals the C ABI of both. Together they show the
//! generated types have the layout a C driver on a 32-bit guest has.
//!
//! Types the private XMLs define (the Venus ring and stream structs) are not
//! in `vulkan.h`; their layouts rest on the model alone.

mod support;

use std::process::Command;

use askama::Template;
use helios_venus_gen::analysis::Analysis;
use helios_venus_gen::emit::type_set;
use helios_venus_gen::layout::Layouts;
use helios_venus_gen::registry::{Category, Registry};

struct Member {
    name: String,
    offset: u64,
}

struct Aggregate {
    name: String,
    size: u64,
    align: u64,
    members: Vec<Member>,
}

#[derive(Template)]
#[template(path = "layout_assertions.c", escape = "none")]
struct Assertions {
    pointer_width: u64,
    aggregates: Vec<Aggregate>,
}

fn aggregates(an: &Analysis, pointer_width: u64) -> (Vec<Aggregate>, usize) {
    let layouts = Layouts::new(&an.reg, pointer_width);
    let set = type_set(an).expect("the type set");
    let mut out = Vec::new();
    let mut private = 0;
    for &id in &set.full {
        let ty = &an.reg.types[id];
        if ty.is_private {
            private += 1;
            continue;
        }
        let layout = layouts.of_aggregate(id).expect("a layout");
        let members = if ty.category() == Category::Struct {
            ty.variables
                .iter()
                .zip(&layout.offsets)
                .map(|(var, &offset)| Member {
                    name: var.name.clone(),
                    offset,
                })
                .collect()
        } else {
            Vec::new()
        };
        out.push(Aggregate {
            name: ty.name.clone(),
            size: layout.layout.size,
            align: layout.layout.align,
            members,
        });
    }
    (out, private)
}

/// Runs `clang` over the assertions for `target` (`None` for the host).
fn check(
    an: &Analysis,
    include: &std::path::Path,
    work: &std::path::Path,
    target: Option<&str>,
    pointer_width: u64,
) -> usize {
    let (aggregates, private) = aggregates(an, pointer_width);
    let count = aggregates.len();
    let source = Assertions {
        pointer_width,
        aggregates,
    }
    .render()
    .expect("the assertions render");
    let file = work.join(format!("layout_{pointer_width}.c"));
    std::fs::write(&file, source).expect("the assertions are written");
    let mut clang = Command::new("clang");
    clang.arg("-fsyntax-only").arg("-I").arg(include);
    if let Some(target) = target {
        // Freestanding: only the compiler's own `stddef.h` and `stdint.h`.
        clang
            .arg(format!("--target={target}"))
            .arg("-ffreestanding");
    }
    clang.arg(&file);
    let output = clang.output().unwrap_or_else(|error| {
        panic!("cannot run clang (it must support the wasm32 target): {error}")
    });
    assert!(
        output.status.success(),
        "clang rejects the layout the model computes for {pointer_width}-byte pointers ({private} private types left out):\n{}",
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .take(40)
            .collect::<Vec<_>>()
            .join("\n")
    );
    count
}

#[test]
fn the_layout_model_equals_the_c_abi_of_wasm32_and_the_host() {
    let inputs = support::inputs();
    let registry = Registry::parse(
        &inputs.path("xmls/vk.xml"),
        &[
            &inputs.path("xmls/VK_EXT_command_serialization.xml"),
            &inputs.path("xmls/VK_MESA_venus_protocol.xml"),
        ],
    )
    .expect("the registry parses");
    let an = Analysis::new(registry).expect("the registry is analysed");
    let work = tempfile::tempdir().expect("a work directory");
    let include = inputs.path("include");

    // wasm32: 4-byte pointers, 8-byte alignment for 64-bit integers.
    let wasm = check(
        &an,
        &include,
        work.path(),
        Some("wasm32-unknown-unknown"),
        4,
    );
    // The host is the 64-bit target the parity test runs on.
    let host = check(&an, &include, work.path(), None, 8);
    assert_eq!(wasm, host);
    assert!(wasm > 500, "{wasm} structs and unions were checked");
}
