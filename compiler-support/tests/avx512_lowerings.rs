//! Pins the three Cranelift properties the x86 backend's vector-state design
//! rests on (`x86/src/extended_state.rs`, `docs/wasmtime.md`):
//!
//! - A: every EVEX instruction Cranelift emits is 128 bits wide, so no `ymm`
//!   or `zmm` operand appears.
//! - B: EVEX masking is always k0, so no `k1`-`k7` operand appears.
//! - C: the allocator uses `xmm0`-`xmm15` only, so no `xmm16`-`xmm31` operand
//!   appears.
//!
//! The kernel preserves guest vector state across a stack-limit interruption
//! by saving 128 bits of `xmm0`-`xmm15` in a `preserve_all` trampoline and
//! across interrupts by leaving the upper state untouched. A fork move that
//! breaks any property makes this test fail.

use cranelift_codegen::settings::Configurable;
use cranelift_codegen::{Context, isa, settings};
use helios_artifact::cwasm_target_isa_levels;

const X86_TARGET: &str = "x86_64-unknown-none";
const LOWERINGS: &str = include_str!("data/avx512-lowerings.clif");

/// Compiles every function of `avx512-lowerings.clif` with every flag of the
/// top x86 level enabled and returns the disassembly of each, by name.
fn disassemble_top_level() -> Vec<(String, String)> {
    let top = cwasm_target_isa_levels(X86_TARGET)
        .iter()
        .last()
        .expect("the x86 target defines at least one level");
    let mut isa_builder = isa::lookup_by_name("x86_64").expect("cranelift is built with x86");
    for feature in top.features() {
        isa_builder
            .enable(feature.cranelift_flag())
            .unwrap_or_else(|error| {
                panic!(
                    "level {} flag {} is not an x86 setting: {error}",
                    top.name(),
                    feature.cranelift_flag()
                )
            });
    }
    let isa = isa_builder
        .finish(settings::Flags::new(settings::builder()))
        .expect("the top x86 level's flags form a valid ISA");
    let capstone = isa.to_capstone().expect("capstone supports x86_64");

    let functions =
        cranelift_reader::parse_functions(LOWERINGS).expect("avx512-lowerings.clif parses");
    functions
        .into_iter()
        .map(|function| {
            let name = function.name.to_string();
            let params = function.params.clone();
            let mut context = Context::for_function(function);
            context.set_disasm(true);
            let compiled = context
                .compile(&*isa, &mut Default::default())
                .unwrap_or_else(|error| panic!("{name} does not compile: {:?}", error.inner));
            let disassembly = compiled
                .disassemble(Some(&params), &capstone)
                .unwrap_or_else(|error| panic!("{name} does not disassemble: {error}"));
            (name, disassembly)
        })
        .collect()
}

/// The registers and vector lengths a wider-than-128-bit or high-register
/// instruction would name. Capstone prints AT&T operands as `%name`.
fn forbidden_operand(line: &str) -> Option<String> {
    let mut rest = line;
    while let Some(at) = rest.find('%') {
        rest = &rest[at + 1..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(rest.len());
        let register = &rest[..end];
        let forbidden = register.starts_with("ymm")
            || register.starts_with("zmm")
            || matches!(register, "k1" | "k2" | "k3" | "k4" | "k5" | "k6" | "k7")
            || register
                .strip_prefix("xmm")
                .and_then(|index| index.parse::<u8>().ok())
                .is_some_and(|index| index >= 16);
        if forbidden {
            return Some(register.to_owned());
        }
    }
    None
}

#[test]
fn top_level_emits_only_128_bit_evex_in_xmm0_to_15_with_k0() {
    let compiled = disassemble_top_level();
    assert_eq!(compiled.len(), 8, "avx512-lowerings.clif changed shape");

    for (name, disassembly) in &compiled {
        for line in disassembly.lines() {
            if let Some(register) = forbidden_operand(line) {
                panic!("{name}: `%{register}` operand in `{line}`\n{disassembly}");
            }
        }
    }

    // Each operation must reach the EVEX form the AVX-512 flags enable; a
    // lowering that silently falls back to SSE/VEX would leave properties A-C
    // unexercised for it.
    let expected = [
        ("%imul_i64x2", "vpmullq"),
        ("%abs_i64x2", "vpabsq"),
        ("%sshr_i64x2", "vpsraq"),
        ("%sshr_imm_i64x2", "vpsraq"),
        ("%popcnt_i8x16", "vpopcntb"),
        ("%shuffle_two_inputs", "vpermi2b"),
        ("%fcvt_from_uint_i32x4", "vcvtudq2ps"),
    ];
    for (function, mnemonic) in expected {
        let (_, disassembly) = compiled
            .iter()
            .find(|(name, _)| name == function)
            .unwrap_or_else(|| panic!("{function} is missing from avx512-lowerings.clif"));
        assert!(
            disassembly.contains(mnemonic),
            "{function} did not lower to {mnemonic}:\n{disassembly}"
        );
    }
}

#[test]
fn forbidden_operands_are_recognised() {
    assert_eq!(
        forbidden_operand("vpmullq %ymm1, %ymm0, %ymm0").as_deref(),
        Some("ymm1")
    );
    assert_eq!(
        forbidden_operand("vaddpd %zmm2, %xmm0, %xmm0").as_deref(),
        Some("zmm2")
    );
    assert_eq!(
        forbidden_operand("vpabsq %xmm16, %xmm0").as_deref(),
        Some("xmm16")
    );
    assert_eq!(
        forbidden_operand("vpabsq %xmm3, %xmm31").as_deref(),
        Some("xmm31")
    );
    assert_eq!(
        forbidden_operand("vpabsq %xmm3, %xmm0{%k1}").as_deref(),
        Some("k1")
    );
    assert_eq!(forbidden_operand("vpmullq %xmm15, %xmm1, %xmm0"), None);
    assert_eq!(forbidden_operand("movq %rsp, %rbp"), None);
}
