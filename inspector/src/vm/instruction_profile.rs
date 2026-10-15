//! Writing the guest's instruction-address samples as a folded profile.
//!
//! The kernel samples the instruction each processor's periodic timer
//! interrupted and names the wasm ones itself, because only it knows
//! which programs were loaded where. Everything else it hands back as a
//! bare address: the kernel image carries no symbol table at runtime,
//! and this host holds the exact ELF it booted. So the folded file is
//! assembled here, with the native addresses named against that image
//! at the base the guest reports it was loaded at.
//!
//! Each line is one location and its sample count: `wasm;<program>;<function>`,
//! `wasm;<program>;[trampoline]`, `kernel;<function>`, or `unknown` for an
//! address in neither.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use askama::Template as _;
use helios_inspector_protocol::system::profiling::{
    self as system_profiling, InstructionProfile, InstructionProfileError, SampledCode,
};

use super::kernel_image::{KernelSymbols, KernelSymbolsError, LoadedKernelSymbols};
use super::{FoldedProfileLine, FoldedProfileTemplate};
use crate::serial::RpcClient;

/// Why the instruction profile a run asked for was not written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InstructionProfileOutputError {
    #[error(
        "failed to {} the guest's instruction sampling: {source}",
        if *.enabled { "start" } else { "stop" }
    )]
    Control {
        enabled: bool,
        #[source]
        source: helios_inspector_protocol::RpcError,
    },
    #[error("failed to read the guest's instruction samples: {source}")]
    Read {
        #[source]
        source: helios_inspector_protocol::RpcError,
    },
    #[error("the guest kernel cannot produce an instruction profile: {}", refusal_reason(*.refusal))]
    Refused { refusal: InstructionProfileError },
    #[error("cannot name the kernel addresses of the instruction profile: {source}")]
    Symbols {
        #[source]
        source: KernelSymbolsError,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to report the written instruction profile: {source}")]
    Report {
        #[source]
        source: std::io::Error,
    },
}

fn refusal_reason(refusal: InstructionProfileError) -> &'static str {
    match refusal {
        InstructionProfileError::NotCaptured => {
            "this architecture's backend does not sample instruction addresses (only x86-64 does)"
        }
    }
}

/// Starts the guest's instruction sampling for the workload that follows.
///
/// Sampling is its own control, separate from the scoped profiler, because
/// it shortens every guest processor's timer period and so perturbs the
/// timings it runs under: only a run that asked for an instruction profile
/// starts it. Starting discards the samples of any earlier run.
pub(crate) async fn start(client: &RpcClient) -> Result<(), InstructionProfileOutputError> {
    set_sampling(client, true).await
}

/// Stops the guest's instruction sampling, once the workload is done and
/// before its samples are read.
pub(crate) async fn stop(client: &RpcClient) -> Result<(), InstructionProfileOutputError> {
    set_sampling(client, false).await
}

async fn set_sampling(
    client: &RpcClient,
    enabled: bool,
) -> Result<(), InstructionProfileOutputError> {
    system_profiling::set_instruction_sampling(client, enabled)
        .await
        .map_err(|source| InstructionProfileOutputError::Control { enabled, source })?
        .map_err(|refusal| InstructionProfileOutputError::Refused { refusal })
}

/// Reads the guest's instruction samples, taken between [`start`] and
/// [`stop`], and writes them to `output`, naming native addresses against
/// the kernel image at `kernel`.
pub(crate) async fn collect(
    client: &RpcClient,
    kernel: &Path,
    output: &Path,
) -> Result<(), InstructionProfileOutputError> {
    let profile = system_profiling::instruction_samples(client)
        .await
        .map_err(|source| InstructionProfileOutputError::Read { source })?
        .map_err(|refusal| InstructionProfileOutputError::Refused { refusal })?;
    let kernel = kernel.to_path_buf();
    let output = output.to_path_buf();
    // The kernel image is tens of megabytes; reading and indexing it
    // does not belong on the executor thread.
    blocking::unblock(move || write(&profile, &kernel, &output)).await
}

fn write(
    profile: &InstructionProfile,
    kernel: &Path,
    output: &PathBuf,
) -> Result<(), InstructionProfileOutputError> {
    use std::io::Write as _;

    let symbols = KernelSymbols::read(kernel)
        .map_err(|source| InstructionProfileOutputError::Symbols { source })?;
    let folded = render(profile, &symbols.loaded_at(profile.kernel_image_base));
    std::fs::write(output, folded).map_err(|source| InstructionProfileOutputError::Write {
        path: output.display().to_string(),
        source,
    })?;
    let samples = profile
        .samples
        .iter()
        .map(|sample| sample.count)
        .sum::<u64>();
    let mut stderr = std::io::stderr().lock();
    writeln!(
        stderr,
        "instruction_profile_output={} samples={samples} dropped={} period_ns={}",
        output.display(),
        profile.dropped,
        profile.period_nanos,
    )
    .map_err(|source| InstructionProfileOutputError::Report { source })
}

/// The folded text of `profile`, heaviest location first.
fn render(profile: &InstructionProfile, kernel: &LoadedKernelSymbols<'_>) -> String {
    let mut stacks = BTreeMap::<String, u64>::new();
    for sample in &profile.samples {
        *stacks.entry(stack(&sample.code, kernel)).or_insert(0) += sample.count;
    }
    let mut lines = stacks
        .into_iter()
        .map(|(stack, weight)| FoldedProfileLine { stack, weight })
        .collect::<Vec<_>>();
    lines.sort_by(|left, right| {
        right
            .weight
            .cmp(&left.weight)
            .then_with(|| left.stack.cmp(&right.stack))
    });
    FoldedProfileTemplate { lines }
        .render()
        .expect("folded profile template rendering is infallible")
}

fn stack(code: &SampledCode, kernel: &LoadedKernelSymbols<'_>) -> String {
    match code {
        SampledCode::Wasm(function) => {
            let name = match &function.name {
                // A Rust program's name section may carry mangled
                // symbols; anything else demangles to itself.
                Some(name) => frame(&format!("{:#}", rustc_demangle::demangle(name))),
                None => format!("func[{}:{}]", function.static_module, function.index),
            };
            format!("wasm;{};{name}", frame(&function.module))
        }
        SampledCode::WasmTrampoline(module) => format!("wasm;{};[trampoline]", frame(module)),
        SampledCode::Native(address) if kernel.contains_text(*address) => {
            match kernel.symbolize(*address) {
                Some(symbol) => format!("kernel;{}", frame(&symbol.function)),
                None => "kernel;[unsymbolized]".to_owned(),
            }
        }
        SampledCode::Native(_) => "unknown".to_owned(),
    }
}

/// One frame of a folded stack: `;` separates frames and a line ends the
/// sample, so neither may appear inside one.
fn frame(name: &str) -> String {
    name.replace([';', '\n', '\r'], "_")
}

#[cfg(test)]
mod tests {
    use helios_inspector_protocol::system::profiling::{
        InstructionProfile, InstructionSample, SampledCode, WasmFunction,
    };

    use super::render;
    use crate::vm::kernel_image::{Function, KernelSymbols};

    #[test]
    fn samples_fold_into_wasm_kernel_and_unknown_frames() {
        let text = 0xffff_ffff_8000_1000..0xffff_ffff_8000_2000;
        let symbols = KernelSymbols::new(
            0xffff_ffff_8000_0000,
            vec![text],
            vec![
                Function {
                    name: "_RNvNtCs1234_13helios_kernel4exec4park".to_owned(),
                    address: 0xffff_ffff_8000_1000,
                    size: 0x100,
                },
                Function {
                    name: "rdtsc_now".to_owned(),
                    address: 0xffff_ffff_8000_1100,
                    size: 0x10,
                },
            ],
        );
        let function = |name: Option<&str>, index| {
            SampledCode::Wasm(WasmFunction {
                module: "python3;bad".to_owned(),
                static_module: 1,
                index,
                name: name.map(str::to_owned),
            })
        };
        let profile = InstructionProfile {
            period_nanos: 1_000_000,
            kernel_image_base: 0xffff_ffff_8120_0000,
            samples: vec![
                InstructionSample {
                    code: function(Some("_PyEval_EvalFrameDefault"), 7),
                    count: 40,
                },
                InstructionSample {
                    code: SampledCode::Native(0xffff_ffff_8120_1004),
                    count: 12,
                },
                InstructionSample {
                    code: SampledCode::Native(0xffff_ffff_8120_1008),
                    count: 3,
                },
                InstructionSample {
                    code: SampledCode::Native(0xffff_ffff_8120_1104),
                    count: 5,
                },
                InstructionSample {
                    code: function(None, 9),
                    count: 2,
                },
                InstructionSample {
                    code: SampledCode::WasmTrampoline("python3;bad".to_owned()),
                    count: 2,
                },
                InstructionSample {
                    code: SampledCode::Native(0x4000),
                    count: 1,
                },
            ],
            dropped: 0,
        };

        let folded = render(&profile, &symbols.loaded_at(profile.kernel_image_base));

        assert_eq!(
            folded,
            "wasm;python3_bad;_PyEval_EvalFrameDefault 40\n\
             kernel;helios_kernel::exec::park 15\n\
             kernel;rdtsc_now 5\n\
             wasm;python3_bad;[trampoline] 2\n\
             wasm;python3_bad;func[1:9] 2\n\
             unknown 1\n"
        );
    }
}
