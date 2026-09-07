//! The branch-hint loop end to end: instrument a module, run it, read its
//! counts back, and write the hints they justify into the original.

use helios_branch_hints::{
    counts::Counts,
    hint,
    instrument::instrument,
    module::CoreModule,
    profile::{Profile, SiteCounts},
};
use wasmtime::{Caller, Config, Engine, Extern, Linker, Module, Store};

/// A module with one `if` and one `br_if`, both in the same loop, so the
/// two counter pairs are known exactly: the `if` sees ten executions split
/// evenly, and the `br_if` nine taken out of ten.
const COUNTED: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (func $work (param $n i32) (result i32)
    (local $i i32) (local $acc i32)
    (loop $l
      (if (i32.eqz (i32.rem_u (local.get $i) (i32.const 2)))
        (then (local.set $acc (i32.add (local.get $acc) (i32.const 1))))
        (else (local.set $acc (i32.add (local.get $acc) (i32.const 2)))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (local.get $n))))
    (local.get $acc))
  (func $_start (export "_start")
    (drop (call $work (i32.const 10)))))
"#;

/// The smallest module that can carry one hint: one function, one `if`,
/// and a body whose bytes are short enough to write the expected section
/// out by hand.
const ONE_BRANCH: &str = r#"
(module
  (func (param i32)
    local.get 0
    if
    end))
"#;

#[test]
fn instrumented_counts_land_where_the_program_branched() {
    let original = wat::parse_str(COUNTED).expect("the probe module assembles");
    let instrumented = instrument(&original, "COUNTED").expect("the probe module instruments");
    assert_eq!(
        instrumented.sites.sites.len(),
        2,
        "the probe module has one `if` and one `br_if`"
    );

    let output = run(&instrumented.wasm);
    let counts = Counts::parse(&output).expect("the run dumps a framed profile");
    let profile = Profile::record(
        &instrumented.sites,
        "COUNTED",
        vec!["probe".to_owned()],
        &[counts],
    )
    .expect("the counts match the site map");

    // Both sites are below the observation floor, so the profile drops
    // them; read the raw sums instead.
    assert!(profile.sites.is_empty());
    let mut totals = [(0u64, 0u64); 2];
    for record in &Counts::parse(&output).expect("the dump parses").records {
        totals[usize::try_from(record.site).expect("two sites")] = (record.taken, record.not_taken);
    }
    assert_eq!(totals[0], (5, 5), "the `if` alternates over ten iterations");
    assert_eq!(totals[1], (9, 1), "the `br_if` falls through once");
}

#[test]
fn the_hint_section_is_the_bytes_the_proposal_specifies() {
    let module = wat::parse_str(ONE_BRANCH).expect("the one-branch module assembles");
    let fingerprint = CoreModule::parse(&module)
        .expect("the one-branch module parses")
        .code_fingerprint();
    let profile = Profile {
        version: 1,
        module: "ONE_BRANCH".to_owned(),
        core_module_index: 0,
        code_fingerprint: fingerprint,
        runs: vec!["hand".to_owned()],
        min_observations: 1_000,
        sites: vec![SiteCounts {
            func: 0,
            // One byte of locals declaration plus `local.get 0`.
            offset: 3,
            taken: 9_500,
            not_taken: 500,
        }],
    };

    let hinted = hint::apply(&module, &profile).expect("the one-branch module hints");
    assert_eq!(hinted.stats.hints, 1);

    let mut expected = vec![
        0x00, // custom section
        0x20, // 32 bytes of contents
        0x19, // a 25-byte name
    ];
    expected.extend_from_slice(b"metadata.code.branch_hint");
    expected.extend_from_slice(&[
        0x01, // one function carries hints
        0x00, // function 0
        0x01, // one hint in it
        0x03, // at body offset 3
        0x01, // whose payload is one byte
        0x01, // meaning "taken"
    ]);

    let section = find_section(&hinted.wasm, &expected);
    assert_eq!(section, expected);
}

#[test]
fn a_hinted_module_reads_back_and_compiles_with_hints_enabled() {
    let module = wat::parse_str(ONE_BRANCH).expect("the one-branch module assembles");
    let fingerprint = CoreModule::parse(&module)
        .expect("the one-branch module parses")
        .code_fingerprint();
    let profile = Profile {
        version: 1,
        module: "ONE_BRANCH".to_owned(),
        core_module_index: 0,
        code_fingerprint: fingerprint,
        runs: vec!["hand".to_owned()],
        min_observations: 1_000,
        sites: vec![SiteCounts {
            func: 0,
            offset: 3,
            taken: 100,
            not_taken: 9_900,
        }],
    };
    let hinted = hint::apply(&module, &profile).expect("the one-branch module hints");

    // The proposal rides in a custom section, so wasmparser has no feature
    // to turn on for it: validation must accept the module and the section
    // reader must give back what was written.
    wasmparser::Validator::new()
        .validate_all(&hinted.wasm)
        .expect("a hinted module is still a valid module");

    let mut read = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&hinted.wasm) {
        if let wasmparser::Payload::CustomSection(section) = payload.expect("the module parses")
            && let wasmparser::KnownCustom::BranchHints(reader) = section.as_known()
        {
            for function in reader {
                let function = function.expect("the hint section decodes");
                for entry in function.hints.clone() {
                    read.push((function.func, entry.expect("a hint decodes")));
                }
            }
        }
    }
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].0, 0);
    assert_eq!(read[0].1.func_offset, 3);
    assert!(!read[0].1.taken);

    // The gate that actually matters: the vendored Wasmtime parses the
    // section only when `wasm_branch_hinting` is on, which is what
    // `helios-compiler-support` turns on for the compiler plugin.
    let mut config = Config::new();
    config.wasm_branch_hinting(true);
    let engine = Engine::new(&config).expect("an engine with branch hinting builds");
    Module::new(&engine, &hinted.wasm).expect("a hinted module compiles with hints enabled");
}

/// Runs an instrumented module far enough to produce its dump, serving
/// `fd_write` from the test rather than from a wasi implementation: the
/// dump is the only thing the probe module writes.
fn run(wasm: &[u8]) -> String {
    let engine = Engine::default();
    let module = Module::new(&engine, wasm).expect("the instrumented module compiles");
    let mut linker: Linker<Vec<u8>> = Linker::new(&engine);
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_write",
            |mut caller: Caller<'_, Vec<u8>>,
             _fd: i32,
             iovs: i32,
             iovs_len: i32,
             written: i32|
             -> i32 {
                let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
                    panic!("the instrumented module exports its memory");
                };
                let data = memory.data(&caller);
                let mut total = 0u32;
                let mut collected = Vec::new();
                for index in 0..iovs_len {
                    let entry = usize::try_from(iovs + index * 8).expect("a small address");
                    let pointer =
                        u32::from_le_bytes(data[entry..entry + 4].try_into().expect("four bytes"));
                    let length = u32::from_le_bytes(
                        data[entry + 4..entry + 8].try_into().expect("four bytes"),
                    );
                    let start = usize::try_from(pointer).expect("a small address");
                    let end = start + usize::try_from(length).expect("a small length");
                    collected.extend_from_slice(&data[start..end]);
                    total += length;
                }
                caller.data_mut().extend_from_slice(&collected);
                let slot = usize::try_from(written).expect("a small address");
                memory.data_mut(&mut caller)[slot..slot + 4].copy_from_slice(&total.to_le_bytes());
                0
            },
        )
        .expect("fd_write is definable");
    linker
        .func_wrap("wasi_snapshot_preview1", "proc_exit", |_code: i32| {})
        .expect("proc_exit is definable");

    let mut store = Store::new(&engine, Vec::new());
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("the instrumented module instantiates");
    instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .expect("the wrapper keeps the `_start` export")
        .call(&mut store, ())
        .expect("the probe module runs");
    String::from_utf8(store.into_data()).expect("the dump is ascii")
}

fn find_section(wasm: &[u8], expected: &[u8]) -> Vec<u8> {
    let name = b"metadata.code.branch_hint";
    let at = wasm
        .windows(name.len())
        .position(|window| window == name)
        .expect("the hinted module carries the section");
    // Back up over the name length, the section length and the section id.
    let start = at - 3;
    wasm[start..start + expected.len()].to_vec()
}
