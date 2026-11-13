//! The host's half of the compiler-plugin ABI, against a stand-in module
//! (`fixtures/abi.wat`) that implements the ABI and echoes back what it was
//! sent.

use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroU32;
use std::process::{Command, Stdio};

use helios_artifact::IsaLevelError;
use helios_compiler_abi::{CompileHint, CompilerStatus, OutputKind};
use helios_compiler_host::{Compiled, CompilerHost, CompilerHostError, on_wasm_thread};

const TARGET: &str = "x86_64-unknown-none";
/// A level `TARGET` defines whose name carries `+` extras, which the request
/// must carry whole.
const ISA_LEVEL: &str = "x86-64-v4+vbmi+bitalg";

fn stand_in() -> Vec<u8> {
    wat::parse_str(include_str!("fixtures/abi.wat")).expect("the stand-in plugin assembles")
}

fn workers(count: u32) -> NonZeroU32 {
    NonZeroU32::new(count).expect("a positive worker count")
}

/// One host, the given compiles in order on one wasm thread, every result.
fn compile_all(
    plugin: Vec<u8>,
    workers: NonZeroU32,
    inputs: Vec<Vec<u8>>,
) -> Result<Vec<Result<Compiled, CompilerHostError>>, CompilerHostError> {
    on_wasm_thread(move || -> Result<_, CompilerHostError> {
        let mut host = CompilerHost::new(&plugin, workers)?;
        Ok(inputs
            .iter()
            .map(|input| host.compile(input, TARGET, ISA_LEVEL, CompileHint::Performance))
            .collect())
    })?
}

/// What the stand-in answers a well-formed request with.
fn echoed(workers: u32, input: &[u8]) -> Vec<u8> {
    let mut expected = u32::from(CompileHint::Performance).to_le_bytes().to_vec();
    expected.extend_from_slice(format!("RAYON_NUM_THREADS={workers}\0").as_bytes());
    expected.extend_from_slice(TARGET.as_bytes());
    expected.extend_from_slice(ISA_LEVEL.as_bytes());
    expected.extend_from_slice(input);
    expected
}

#[test]
fn compiles_run_the_kernels_call_sequence_on_a_resident_runtime() {
    let results = compile_all(
        stand_in(),
        workers(3),
        vec![b"curl".to_vec(), b"second".to_vec()],
    )
    .expect("the stand-in is hosted");
    // The second compile runs on a fresh instance that skipped the memory
    // initialisation and adopted the first instance's TLS block.
    for (result, input) in results.into_iter().zip([&b"curl"[..], &b"second"[..]]) {
        let compiled = result.expect("a well-formed request compiles");
        assert_eq!(compiled.output, echoed(3, input));
        assert_eq!(compiled.kind, OutputKind::CoreModule);
        assert!(compiled.diagnostic.is_empty());
    }
}

#[test]
fn an_isa_level_the_target_does_not_define_is_refused_before_the_plugin_runs() {
    let plugin = stand_in();
    let result = on_wasm_thread(move || -> Result<_, CompilerHostError> {
        let mut host = CompilerHost::new(&plugin, workers(1))?;
        host.compile(b"curl", TARGET, "x86-64-v9", CompileHint::Performance)
    })
    .expect("the driver thread runs");
    match result {
        Err(CompilerHostError::IsaLevel(IsaLevelError::UnknownLevel { name, defined })) => {
            assert_eq!(name, "x86-64-v9");
            assert!(defined.contains(ISA_LEVEL), "{defined}");
        }
        other => panic!("expected an unknown ISA level, got {other:?}"),
    }
}

#[test]
fn a_rejected_request_carries_the_plugins_diagnostic() {
    let results =
        compile_all(stand_in(), workers(2), vec![Vec::new()]).expect("the stand-in is hosted");
    match results.into_iter().next().expect("one result") {
        Err(CompilerHostError::Rejected { status, diagnostic }) => {
            assert_eq!(status, CompilerStatus::CompileFailed);
            assert_eq!(diagnostic, "empty");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[test]
fn a_response_in_another_abi_version_is_refused() {
    let results =
        compile_all(stand_in(), workers(2), vec![vec![2]]).expect("the stand-in is hosted");
    match results.into_iter().next().expect("one result") {
        Err(CompilerHostError::AbiMismatch { found, expected }) => {
            assert_eq!(
                (found, expected),
                (99, helios_compiler_abi::HELIOS_COMPILER_ABI_VERSION)
            );
        }
        other => panic!("expected an ABI mismatch, got {other:?}"),
    }
}

/// A response whose enumerated field names no variant is refused with the
/// field's name, and the buffers the host adopted from it were still freed:
/// otherwise the outstanding-bytes check would fail first.
#[test]
fn an_unknown_status_is_refused_after_its_buffers_are_freed() {
    let results =
        compile_all(stand_in(), workers(2), vec![vec![3]]).expect("the stand-in is hosted");
    match results.into_iter().next().expect("one result") {
        Err(CompilerHostError::UnknownDiscriminant(unknown)) => {
            assert_eq!((unknown.field, unknown.value), ("status", 77));
        }
        other => panic!("expected an unknown status, got {other:?}"),
    }
}

#[test]
fn an_unknown_output_kind_is_refused_after_its_buffers_are_freed() {
    let results =
        compile_all(stand_in(), workers(2), vec![vec![4]]).expect("the stand-in is hosted");
    match results.into_iter().next().expect("one result") {
        Err(CompilerHostError::UnknownDiscriminant(unknown)) => {
            assert_eq!((unknown.field, unknown.value), ("output_kind", 55));
        }
        other => panic!("expected an unknown output kind, got {other:?}"),
    }
}

#[test]
fn a_count_left_off_its_starting_level_fails_the_compile() {
    let results =
        compile_all(stand_in(), workers(2), vec![vec![5]]).expect("the stand-in is hosted");
    match results.into_iter().next().expect("one result") {
        Err(CompilerHostError::BuffersOutstanding { before, after }) => {
            assert_eq!((before, after), (0, 7));
        }
        other => panic!("expected outstanding bytes, got {other:?}"),
    }
}

#[test]
fn a_plugin_thread_that_traps_fails_the_compile() {
    let results =
        compile_all(stand_in(), workers(2), vec![vec![1]]).expect("the stand-in is hosted");
    match results.into_iter().next().expect("one result") {
        Err(CompilerHostError::ThreadTrap { thread_id, .. }) => assert_eq!(thread_id, 1),
        other => panic!("expected a thread trap, got {other:?}"),
    }
}

#[test]
fn a_plugin_whose_start_publishes_no_tls_block_is_refused() {
    let plugin = wat::parse_str(
        r#"(module (import "env" "memory" (memory 1 1 shared)) (global (export "__tls_base") (mut i32) (i32.const 0)))"#,
    )
    .expect("the module assembles");
    let results = compile_all(plugin, workers(1), vec![b"curl".to_vec()]).expect("it precompiles");
    let result = results.into_iter().next().expect("one result");
    assert!(
        matches!(result, Err(CompilerHostError::TlsBaseUnset)),
        "{result:?}"
    );
}

#[test]
fn a_plugin_without_a_shared_memory_is_refused() {
    let plugin = wat::parse_str(r#"(module (import "env" "memory" (memory 1 1)))"#)
        .expect("the module assembles");
    let results = compile_all(plugin, workers(1), vec![b"curl".to_vec()]).expect("it precompiles");
    let result = results.into_iter().next().expect("one result");
    assert!(
        matches!(result, Err(CompilerHostError::MemoryContract { .. })),
        "{result:?}"
    );
}

#[test]
fn the_resident_binary_answers_one_line_per_request() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let plugin = scratch.path().join("compiler_plugin.wasm");
    let input = scratch.path().join("input.wasm");
    let output = scratch.path().join("output.cwasm");
    std::fs::write(&plugin, stand_in()).expect("the plugin is written");
    std::fs::write(&input, b"curl").expect("the input is written");

    let run = |requests: &[u8]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_helios-compiler-host"))
            .arg("--plugin")
            .arg(&plugin)
            .arg("--input")
            .arg(&input)
            .arg("--output")
            .arg(&output)
            .args([
                "--target",
                TARGET,
                "--isa-level",
                ISA_LEVEL,
                "--hint",
                "performance",
                "--workers",
                "2",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the host starts");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(requests)
            .expect("the requests are written");
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let lines: Vec<serde_json::Value> = stdout
            .lines()
            .map(|line| {
                serde_json::from_str(&line.expect("a reply line")).expect("a JSON reply line")
            })
            .collect();
        let status = child.wait().expect("the host exits");
        (status, lines)
    };

    let (status, lines) = run(b"compile\ncompile\n");
    assert!(status.success(), "{status}");
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[0], serde_json::json!({"type": "ready"}));
    let compiled = serde_json::json!({
        "type": "compiled",
        "output_bytes": echoed(2, b"curl").len(),
        "metrics": {"compiler_workers": 2},
    });
    assert_eq!(lines[1..], [compiled.clone(), compiled]);
    assert_eq!(
        std::fs::read(&output).expect("the output is written"),
        echoed(2, b"curl")
    );

    let (status, lines) = run(b"recompile\n");
    assert!(!status.success(), "an unknown request must fail the host");
    assert_eq!(lines, [serde_json::json!({"type": "ready"})]);
}
