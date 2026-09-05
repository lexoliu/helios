use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::{Args as ClapArgs, ValueEnum};
use helios_inspector_protocol::debugger::filesystem as debugger_fs;
use helios_inspector_protocol::system::programs as system_programs;
use serde::{Deserialize, Serialize};

const HOST_HTTP_LARGE_PAYLOAD_FILE: &str = "payload-64m.bin";
const WORKLOAD_MANIFEST_SCHEMA_VERSION: u16 = 2;
/// Prefix of the stdout lines a workload uses to report a secondary
/// measurement, `bench.<name>=<number>`; the Linux runner parses the same
/// lines so every side of the comparison carries the same metric names.
const METRIC_LINE_PREFIX: &str = "bench.";

/// How long one workload iteration may take before the run gives up on
/// it.
///
/// Nothing else bounds it. A workload runs inside the guest and is
/// reported by an RPC that simply never answers if the guest stops
/// making progress, so a hung transfer used to hold the lane until the
/// job's own timeout hours later — and a lane that hangs says less
/// about the failure than a lane that fails, while costing far more.
/// The measured medians on a KVM lane are milliseconds and the largest
/// workload moves 64 MiB, so three minutes is orders of magnitude of
/// headroom over anything healthy and still turns a hang into a named
/// failure inside one step.
pub(crate) const DEFAULT_WORKLOAD_TIMEOUT_SECONDS: u32 = 180;

/// How long the post-failure network-counter read may take.
///
/// Short because it runs after a failure that may itself be a guest that
/// stopped answering: the counters are worth a few seconds of a lane's
/// time and not a minute of it.
const NETWORK_COUNTER_DEADLINE_SECONDS: u32 = 10;

/// A workload that never came back.
///
/// Typed rather than a bare message because the deadline is the one
/// failure the runner reports about a workload it has no output for:
/// the RPC never answered, so there is nothing to validate and nothing
/// to print, and what the lane needs to read is which workload, which
/// iteration, and how long it was given.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkloadBenchError {
    #[error(
        "workload {workload} iteration {iteration} did not finish within {seconds}s; \
         the guest never answered, so neither of its streams was captured"
    )]
    WorkloadTimedOut {
        workload: String,
        iteration: u16,
        seconds: u32,
    },
    #[error(
        "the guest did not answer {step} within {seconds}s; the run's workloads are \
         already recorded, so the guest is torn down rather than waited on"
    )]
    GuestStepTimedOut { step: &'static str, seconds: u32 },
}

#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct WorkloadBenchCommand {
    /// Workload manifest shared with the Linux runner.
    #[arg(long, default_value = "tools/wasi-apps/workloads.json")]
    manifest: PathBuf,

    /// Number of measured executions per workload.
    #[arg(long, default_value_t = 5)]
    iterations: u16,

    /// Restrict the run to named workloads. Repeat for multiple workloads.
    #[arg(long = "workload")]
    workloads: Vec<String>,

    /// Restrict the run to workload classes. Repeat for multiple classes.
    #[arg(long = "class", value_enum)]
    classes: Vec<WorkloadClass>,

    /// Host HTTP URL visible from inside the VM for workloads requiring host networking.
    #[arg(long)]
    host_http_url: Option<String>,

    /// Host TCP address visible from inside the VM for raw TCP throughput workloads.
    #[arg(long)]
    host_tcp_host: Option<String>,

    /// Host TCP port visible from inside the VM for raw TCP throughput workloads.
    #[arg(long)]
    host_tcp_port: Option<u16>,

    /// Host TCP echo port visible from inside the VM for round-trip latency workloads.
    #[arg(long)]
    host_tcp_echo_port: Option<u16>,

    /// Record a workload that fails as a `failure` record and go on to the
    /// next one, instead of ending the run at the first failure. The
    /// benchmark suite runs this way so every cell of its report is
    /// accounted for; the exit status stays zero and the JSONL is the record.
    #[arg(long)]
    pub(crate) keep_going: bool,

    /// Write folded kernel/user profile samples collected during the workload run.
    #[arg(long)]
    pub(crate) profile_output: Option<PathBuf>,

    /// Write folded kernel-only profile samples collected during the workload run.
    #[arg(long)]
    pub(crate) kernel_profile_output: Option<PathBuf>,

    /// Write folded user-only profile samples collected during the workload run.
    #[arg(long)]
    pub(crate) user_profile_output: Option<PathBuf>,

    /// Write structured kernel/user perf metrics collected during the workload run.
    #[arg(long)]
    pub(crate) perf_metrics_output: Option<PathBuf>,

    /// Seconds one workload iteration may take before the run fails it.
    #[arg(long, default_value_t = DEFAULT_WORKLOAD_TIMEOUT_SECONDS)]
    pub(crate) workload_timeout_seconds: u32,
}

/// The design claim a workload isolates; `docs/benchmarks.md` describes
/// each class and the Linux counterpart it is compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum WorkloadClass {
    Startup,
    Hostcall,
    Ipc,
    Sched,
    Net,
    Fs,
    Compute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WorkloadRunner {
    Shell,
    Program,
    HeliosAot,
}

#[derive(Debug, Deserialize)]
struct WorkloadManifest {
    schema_version: u16,
    workloads: Vec<Workload>,
}

#[derive(Debug, Clone, Deserialize)]
struct Workload {
    name: String,
    class: WorkloadClass,
    /// Whether the regression gate and the README table carry this workload.
    #[serde(default)]
    headline: bool,
    runner: WorkloadRunner,
    #[serde(rename = "description")]
    _description: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    boot_programs: Vec<String>,
    #[serde(default)]
    stdout_contains: Vec<String>,
    #[serde(default)]
    stderr_empty: bool,
    #[serde(default)]
    requires_host_http: bool,
    #[serde(default)]
    requires_host_tcp: bool,
    #[serde(default)]
    requires_host_tcp_echo: bool,
    #[serde(default)]
    wasm_path: Option<PathBuf>,
    #[serde(default)]
    remote_path: Option<String>,
    #[serde(default)]
    destination_path: Option<String>,
    #[serde(default)]
    throughput_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct VmProvenance {
    pub(crate) arch: &'static str,
    pub(crate) release: bool,
    pub(crate) smp: u16,
    pub(crate) memory: String,
    pub(crate) cpu: Option<String>,
    pub(crate) accel: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JsonlRecord<'a> {
    Run {
        schema_version: u16,
        git_sha: String,
        vm: &'a VmProvenance,
        manifest: String,
        iterations: u16,
        selected_workloads: Vec<String>,
    },
    Iteration {
        workload: &'a str,
        class: WorkloadClass,
        headline: bool,
        runner: WorkloadRunner,
        iteration: u16,
        elapsed_ms: u128,
        /// Secondary measurements the workload printed as `bench.<name>=<number>`.
        metrics: BTreeMap<String, f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        throughput_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        throughput_mib_per_second: Option<f64>,
        stdout: StreamValidation<'a>,
        stderr: StreamValidation<'a>,
        validation: ValidationSummary,
    },
    /// A workload that did not produce a summary: how far it got is in
    /// the iteration records before this one, and `error` says why it
    /// stopped. Written only under `--keep-going`.
    Failure {
        workload: &'a str,
        class: WorkloadClass,
        headline: bool,
        runner: WorkloadRunner,
        error: String,
    },
    Summary {
        workload: &'a str,
        class: WorkloadClass,
        headline: bool,
        runner: WorkloadRunner,
        median_elapsed_ms: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        throughput_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        throughput_mib_per_second: Option<f64>,
        iterations: u16,
        elapsed_ms: Vec<u128>,
        validation: ValidationSummary,
    },
}

#[derive(Debug, Serialize)]
struct StreamValidation<'a> {
    bytes: usize,
    contains: &'a [String],
    contains_ok: bool,
    empty_required: bool,
    empty_ok: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct ValidationSummary {
    ok: bool,
}

pub(crate) fn required_boot_programs(command: &WorkloadBenchCommand) -> Result<Vec<String>> {
    let workloads = select_workloads(command)?;
    let mut programs = vec!["dash".to_owned(), "debugger".to_owned()];
    for workload in workloads {
        if matches!(
            workload.runner,
            WorkloadRunner::Shell | WorkloadRunner::Program
        ) {
            extend_unique(&mut programs, &workload.boot_programs);
        }
    }
    Ok(programs)
}

pub(crate) async fn run_inner(
    client: &mut crate::serial::RpcClient,
    command: &WorkloadBenchCommand,
    provenance: &VmProvenance,
) -> Result<()> {
    if command.iterations == 0 {
        bail!("workload-bench --iterations must be non-zero");
    }
    let manifest_path = command.manifest.clone();
    let workloads = select_workloads(command)?;
    let selected_workloads = workloads
        .iter()
        .map(|workload| workload.name.clone())
        .collect::<Vec<_>>();
    write_record(&JsonlRecord::Run {
        schema_version: 1,
        git_sha: git_sha().unwrap_or_else(|_| "unknown".to_owned()),
        vm: provenance,
        manifest: manifest_path.display().to_string(),
        iterations: command.iterations,
        selected_workloads,
    })?;

    let mut failed = Vec::new();
    let mut remaining = workloads.into_iter();
    while let Some(workload) = remaining.next() {
        let elapsed_ms = match measure_workload(client, &workload, command).await {
            Ok(elapsed_ms) => elapsed_ms,
            Err(error) if guest_panic(&error).is_some() => {
                // The guest kernel is gone: every further workload would
                // measure a corpse. Record what this one and each of the
                // ones behind it never got to measure, so the report
                // carries a failed cell for each instead of silently
                // dropping them, then let the caller see the panic.
                //
                // The cells carry the panic report rather than the error
                // chain: every layer the panic crossed repeats it, and a
                // table cell wants the kernel's sentence once.
                let report = format!(
                    "guest kernel panicked during {}: {}",
                    workload.name,
                    guest_panic(&error).unwrap_or_default()
                );
                eprintln!(
                    "helios-inspector: workload {} killed the guest; recording the rest as failed: {error:#}",
                    workload.name
                );
                for pending in core::iter::once(workload).chain(remaining) {
                    write_record(&JsonlRecord::Failure {
                        workload: &pending.name,
                        class: pending.class,
                        headline: pending.headline,
                        runner: pending.runner,
                        error: report.clone(),
                    })?;
                    failed.push(pending.name);
                }
                report_failed(&failed);
                return Err(error);
            }
            Err(error) if command.keep_going => {
                eprintln!(
                    "helios-inspector: workload {} failed; recorded and continuing: {error:#}",
                    workload.name
                );
                write_record(&JsonlRecord::Failure {
                    workload: &workload.name,
                    class: workload.class,
                    headline: workload.headline,
                    runner: workload.runner,
                    error: format!("{error:#}"),
                })?;
                failed.push(workload.name.clone());
                continue;
            }
            Err(error) => return Err(error),
        };
        let median = median(&elapsed_ms)?;
        write_record(&JsonlRecord::Summary {
            workload: &workload.name,
            class: workload.class,
            headline: workload.headline,
            runner: workload.runner,
            median_elapsed_ms: median,
            throughput_bytes: workload.throughput_bytes,
            throughput_mib_per_second: throughput_mib_per_second(workload.throughput_bytes, median),
            iterations: command.iterations,
            elapsed_ms,
            validation: ValidationSummary { ok: true },
        })?;
    }
    report_failed(&failed);
    Ok(())
}

fn report_failed(failed: &[String]) {
    if failed.is_empty() {
        return;
    }
    eprintln!(
        "helios-inspector: {} workload(s) recorded as failed: {}",
        failed.len(),
        failed.join(", ")
    );
}

/// The guest's panic report when this error is a dead guest.
///
/// A panicked kernel answers no further RPC, so the transport reports it
/// as a typed fault rather than letting the read block until the outer
/// deadline; the bench driver has to tell that apart from a workload
/// that merely failed.
fn guest_panic(error: &anyhow::Error) -> Option<&str> {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<helios_inspector_protocol::RpcError>())
        .find_map(helios_inspector_protocol::RpcError::guest_panic)
}

/// Times every iteration of one workload, writing its iteration records.
async fn measure_workload(
    client: &mut crate::serial::RpcClient,
    workload: &Workload,
    command: &WorkloadBenchCommand,
) -> Result<Vec<u128>> {
    let mut elapsed_ms = Vec::new();
    for iteration in 1..=command.iterations {
        let attempt = match workload.runner {
            WorkloadRunner::Shell => {
                under_deadline(
                    workload,
                    iteration,
                    command,
                    run_shell_workload(client, workload, command),
                )
                .await
            }
            WorkloadRunner::Program => {
                under_deadline(
                    workload,
                    iteration,
                    command,
                    run_program_workload(client, workload, command),
                )
                .await
            }
            WorkloadRunner::HeliosAot => {
                under_deadline(
                    workload,
                    iteration,
                    command,
                    run_aot_workload(client, workload, iteration),
                )
                .await
            }
        };
        let output = match attempt {
            Ok(output) => output,
            Err(error) => {
                write_guest_network_counters(client, workload, iteration).await;
                return Err(error);
            }
        };
        let validation = match validate_output(workload, &output.stdout, &output.stderr)
            .with_context(|| {
                format!(
                    "workload {} iteration {} failed validation",
                    workload.name, iteration
                )
            }) {
            Ok(validation) => validation,
            Err(error) => {
                write_guest_network_counters(client, workload, iteration).await;
                return Err(error);
            }
        };
        elapsed_ms.push(output.elapsed_ms);
        write_record(&JsonlRecord::Iteration {
            workload: &workload.name,
            class: workload.class,
            headline: workload.headline,
            runner: workload.runner,
            iteration,
            elapsed_ms: output.elapsed_ms,
            metrics: parse_metrics(&output.stdout)?,
            throughput_bytes: workload.throughput_bytes,
            throughput_mib_per_second: throughput_mib_per_second(
                workload.throughput_bytes,
                output.elapsed_ms,
            ),
            stdout: stream_validation(workload, &output.stdout, false),
            stderr: stream_validation(workload, &output.stderr, workload.stderr_empty),
            validation,
        })?;
    }
    Ok(elapsed_ms)
}

#[derive(Debug)]
struct WorkloadOutput {
    elapsed_ms: u128,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs one workload iteration under the run's per-iteration deadline.
///
/// The deadline is per iteration rather than per workload because that
/// is the unit the guest actually executes: five iterations of a
/// healthy workload should not buy the sixth a longer hang, and naming
/// the iteration is what says whether a workload failed cold or only
/// after it had already succeeded four times.
///
/// When it elapses the iteration's future is dropped, which abandons
/// the outstanding RPC, and the error propagates out of the run — the
/// caller tears the guest down with it, so nothing is left executing.
async fn under_deadline(
    workload: &Workload,
    iteration: u16,
    command: &WorkloadBenchCommand,
    run: impl Future<Output = Result<WorkloadOutput>>,
) -> Result<WorkloadOutput> {
    let seconds = command.workload_timeout_seconds;
    let Some(result) = crate::runtime::timeout(Duration::from_secs(u64::from(seconds)), run).await
    else {
        return Err(WorkloadBenchError::WorkloadTimedOut {
            workload: workload.name.clone(),
            iteration,
            seconds,
        }
        .into());
    };
    result
}

/// Runs one guest step of the run that is not a workload — the profile
/// hand-off, the tracing fetch, the teardown — under the same deadline a
/// workload iteration gets.
///
/// Every one of those is an RPC to the same guest, and a guest that has
/// stopped answering stops answering all of them. Without a bound here
/// the run's last words are the last workload's, and the process sits on
/// a dead VM until something outside it notices: run 33952047436 spent
/// ninety-five minutes that way, holding QEMU open behind it.
pub(crate) async fn guest_step_under_deadline<T>(
    step: &'static str,
    seconds: u32,
    run: impl Future<Output = Result<T>>,
) -> Result<T> {
    let Some(result) = crate::runtime::timeout(Duration::from_secs(u64::from(seconds)), run).await
    else {
        return Err(WorkloadBenchError::GuestStepTimedOut { step, seconds }.into());
    };
    result
}

async fn run_shell_workload(
    client: &mut crate::serial::RpcClient,
    workload: &Workload,
    command: &WorkloadBenchCommand,
) -> Result<WorkloadOutput> {
    let script = render_helios_template(
        workload
            .command
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("workload {} is missing command", workload.name))?,
        workload,
        command,
    )?;
    let started = Instant::now();
    let output = crate::programs::exec(
        client,
        crate::programs::REMOTE_SHELL_PATH,
        &["-c".to_owned(), script],
    )
    .await
    .with_context(|| format!("failed to run workload {}", workload.name))?;
    let elapsed_ms = started.elapsed().as_millis();
    if output.exit_code != 0 {
        write_guest_output(workload, &output.output.stdout, &output.output.stderr)?;
        bail!(
            "workload {} exited with code {}{}",
            workload.name,
            output.exit_code,
            quoted_stderr(&output.output.stderr)
        );
    }
    Ok(WorkloadOutput {
        elapsed_ms,
        stdout: output.output.stdout,
        stderr: output.output.stderr,
    })
}

async fn run_program_workload(
    client: &mut crate::serial::RpcClient,
    workload: &Workload,
    command: &WorkloadBenchCommand,
) -> Result<WorkloadOutput> {
    let program = render_helios_template(
        workload.program.as_ref().ok_or_else(|| {
            anyhow::anyhow!("program workload {} is missing program", workload.name)
        })?,
        workload,
        command,
    )?;
    let args = workload
        .args
        .iter()
        .map(|arg| render_helios_template(arg, workload, command))
        .collect::<Result<Vec<_>>>()?;
    let started = Instant::now();
    let output = crate::programs::exec(client, &program, &args)
        .await
        .with_context(|| format!("failed to run workload {}", workload.name))?;
    let elapsed_ms = started.elapsed().as_millis();
    if output.exit_code != 0 {
        write_guest_output(workload, &output.output.stdout, &output.output.stderr)?;
        bail!(
            "workload {} exited with code {}{}",
            workload.name,
            output.exit_code,
            quoted_stderr(&output.output.stderr)
        );
    }
    Ok(WorkloadOutput {
        elapsed_ms,
        stdout: output.output.stdout,
        stderr: output.output.stderr,
    })
}

async fn run_aot_workload(
    client: &crate::serial::RpcClient,
    workload: &Workload,
    iteration: u16,
) -> Result<WorkloadOutput> {
    let wasm_path = workload
        .wasm_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("AOT workload {} is missing wasm_path", workload.name))?;
    let remote_path = workload
        .remote_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("AOT workload {} is missing remote_path", workload.name))?;
    let destination_path = workload.destination_path.as_ref().ok_or_else(|| {
        anyhow::anyhow!("AOT workload {} is missing destination_path", workload.name)
    })?;
    let wasm = fs::read(wasm_path)
        .with_context(|| format!("failed to read AOT workload wasm {}", wasm_path.display()))?;
    if iteration == 1 {
        debugger_fs::write(client, remote_path, &wasm, false)
            .await
            .with_context(|| format!("failed to upload {}", wasm_path.display()))?;
    }

    let started = Instant::now();
    let outcome = system_programs::aot(
        client,
        &system_programs::AotRequest {
            source_path: remote_path.clone(),
            destination_path: destination_path.clone(),
            hint: system_programs::AotHint::Performance,
            profile: false,
        },
    )
    .await
    .with_context(|| format!("failed to AOT compile workload {}", workload.name))?;
    outcome.map_err(|error| {
        anyhow::anyhow!(
            "remote AOT workload {} failed: {:?}: {}",
            workload.name,
            error.kind,
            error.detail
        )
    })?;
    Ok(WorkloadOutput {
        elapsed_ms: started.elapsed().as_millis(),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

fn select_workloads(command: &WorkloadBenchCommand) -> Result<Vec<Workload>> {
    let manifest = load_manifest(&command.manifest)?;
    let mut selected = Vec::new();
    for workload in manifest.workloads {
        if !command.workloads.is_empty()
            && !command.workloads.iter().any(|name| name == &workload.name)
        {
            continue;
        }
        if !command.classes.is_empty() && !command.classes.contains(&workload.class) {
            continue;
        }
        validate_workload_shape(&workload)?;
        selected.push(workload);
    }

    if selected.is_empty() {
        bail!("workload selection matched no manifest entries");
    }
    if !command.workloads.is_empty() {
        for requested in &command.workloads {
            if !selected.iter().any(|workload| &workload.name == requested) {
                bail!("unknown or filtered workload {requested}");
            }
        }
    }
    Ok(selected)
}

fn load_manifest(path: &Path) -> Result<WorkloadManifest> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read workload manifest {}", path.display()))?;
    let manifest: WorkloadManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode workload manifest {}", path.display()))?;
    if manifest.schema_version != WORKLOAD_MANIFEST_SCHEMA_VERSION {
        bail!(
            "unsupported workload manifest schema_version {}, expected {}",
            manifest.schema_version,
            WORKLOAD_MANIFEST_SCHEMA_VERSION
        );
    }
    Ok(manifest)
}

fn validate_workload_shape(workload: &Workload) -> Result<()> {
    match workload.runner {
        WorkloadRunner::Shell => {
            if workload.command.is_none() {
                bail!("shell workload {} is missing command", workload.name);
            }
        }
        WorkloadRunner::Program => {
            if workload.program.is_none() {
                bail!("program workload {} is missing program", workload.name);
            }
        }
        WorkloadRunner::HeliosAot => {
            if workload.wasm_path.is_none()
                || workload.remote_path.is_none()
                || workload.destination_path.is_none()
            {
                bail!("AOT workload {} is missing AOT paths", workload.name);
            }
        }
    }
    Ok(())
}

fn render_helios_template(
    template: &str,
    workload: &Workload,
    command: &WorkloadBenchCommand,
) -> Result<String> {
    let mut rendered = template.to_owned();
    for (placeholder, value) in [
        // Scratch files live at the embedded filesystem root inside the
        // guest; the Linux runner maps the same placeholder to a tmpdir.
        ("{workdir}", ""),
        ("{bash}", "/bin/bash"),
        ("{cat}", "/bin/cat"),
        ("{curl}", "/bin/curl"),
        ("{dash}", "/bin/dash"),
        ("{head}", "/bin/head"),
        ("{mkdir}", "/bin/mkdir"),
        ("{python3}", "/bin/python3"),
        ("{quickjs}", "/bin/qjs"),
        ("{simd_lanes}", "/bin/simd-lanes"),
        ("{tcp_throughput}", "/bin/tcp-throughput"),
        ("{wasi_tcp_throughput}", "/bin/wasi-tcp-throughput"),
        ("{wasix_tcp_throughput}", "/bin/wasix-tcp-throughput"),
    ] {
        rendered = rendered.replace(placeholder, value);
    }
    if workload.requires_host_http && command.host_http_url.is_none() {
        bail!(
            "workload {} requires --host-http-url for VM-visible host HTTP",
            workload.name
        );
    }
    if rendered.contains("{host_http_url}") {
        let url = command.host_http_url.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "workload {} requires --host-http-url for VM-visible host HTTP",
                workload.name
            )
        })?;
        rendered = rendered.replace("{host_http_url}", url);
    }
    if rendered.contains("{host_http_large_url}") {
        let url = command.host_http_url.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "workload {} requires --host-http-url for VM-visible host HTTP",
                workload.name
            )
        })?;
        rendered = rendered.replace("{host_http_large_url}", &large_host_http_url(url)?);
    }
    if workload.requires_host_tcp
        && (command.host_tcp_host.is_none() || command.host_tcp_port.is_none())
    {
        bail!(
            "workload {} requires --host-tcp-host and --host-tcp-port for VM-visible host TCP",
            workload.name
        );
    }
    if rendered.contains("{host_tcp_host}") {
        let host = command.host_tcp_host.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "workload {} requires --host-tcp-host for VM-visible host TCP",
                workload.name
            )
        })?;
        rendered = rendered.replace("{host_tcp_host}", host);
    }
    if rendered.contains("{host_tcp_port}") {
        let port = command.host_tcp_port.ok_or_else(|| {
            anyhow::anyhow!(
                "workload {} requires --host-tcp-port for VM-visible host TCP",
                workload.name
            )
        })?;
        rendered = rendered.replace("{host_tcp_port}", &port.to_string());
    }
    if workload.requires_host_tcp_echo
        && (command.host_tcp_host.is_none() || command.host_tcp_echo_port.is_none())
    {
        bail!(
            "workload {} requires --host-tcp-host and --host-tcp-echo-port for VM-visible host TCP echo",
            workload.name
        );
    }
    if rendered.contains("{host_tcp_echo_port}") {
        let port = command.host_tcp_echo_port.ok_or_else(|| {
            anyhow::anyhow!(
                "workload {} requires --host-tcp-echo-port for VM-visible host TCP echo",
                workload.name
            )
        })?;
        rendered = rendered.replace("{host_tcp_echo_port}", &port.to_string());
    }
    Ok(rendered)
}

/// Collects the `bench.<name>=<number>` lines a workload printed.
///
/// A malformed metric line is a workload bug, not noise to skip: the
/// report would silently lose the measurement the workload exists for.
fn parse_metrics(stdout: &[u8]) -> Result<BTreeMap<String, f64>> {
    let mut metrics = BTreeMap::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let Some(assignment) = line.strip_prefix(METRIC_LINE_PREFIX) else {
            continue;
        };
        let (name, value) = assignment
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("metric line {line:?} has no `=`"))?;
        let value = value
            .trim()
            .parse::<f64>()
            .with_context(|| format!("metric line {line:?} has a non-numeric value"))?;
        if metrics.insert(name.trim().to_owned(), value).is_some() {
            bail!("metric {name:?} was reported twice");
        }
    }
    Ok(metrics)
}

fn large_host_http_url(host_http_url: &str) -> Result<String> {
    let (prefix, _) = host_http_url
        .rsplit_once('/')
        .ok_or_else(|| anyhow::anyhow!("host HTTP URL has no path segment: {host_http_url}"))?;
    Ok(format!("{prefix}/{HOST_HTTP_LARGE_PAYLOAD_FILE}"))
}

fn validate_output(workload: &Workload, stdout: &[u8], stderr: &[u8]) -> Result<ValidationSummary> {
    let stdout_text = String::from_utf8_lossy(stdout);
    for expected in &workload.stdout_contains {
        if !stdout_text.contains(expected) {
            write_guest_output(workload, stdout, stderr)?;
            bail!(
                "workload {} stdout did not contain expected text {:?}",
                workload.name,
                expected
            );
        }
    }
    if workload.stderr_empty && !stderr.is_empty() {
        write_guest_output(workload, stdout, stderr)?;
        bail!("workload {} wrote stderr", workload.name);
    }
    Ok(ValidationSummary { ok: true })
}

fn stream_validation<'a>(
    workload: &'a Workload,
    bytes: &[u8],
    empty_required: bool,
) -> StreamValidation<'a> {
    let text = String::from_utf8_lossy(bytes);
    let contains_ok = empty_required
        || workload
            .stdout_contains
            .iter()
            .all(|expected| text.contains(expected));
    StreamValidation {
        bytes: bytes.len(),
        contains: if empty_required {
            &[]
        } else {
            &workload.stdout_contains
        },
        contains_ok,
        empty_required,
        empty_ok: !empty_required || bytes.is_empty(),
    }
}

fn median(values: &[u128]) -> Result<u128> {
    if values.is_empty() {
        bail!("cannot compute median for empty sample set");
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let lower = (sorted.len() - 1) / 2;
    let upper = sorted.len() / 2;
    Ok((sorted[lower] + sorted[upper]) / 2)
}

fn throughput_mib_per_second(bytes: Option<u64>, elapsed_ms: u128) -> Option<f64> {
    let bytes = bytes?;
    if elapsed_ms == 0 {
        return None;
    }
    Some((bytes as f64 / (1024.0 * 1024.0)) / (elapsed_ms as f64 / 1000.0))
}

fn extend_unique(programs: &mut Vec<String>, required: &[String]) {
    for program in required {
        if !programs.iter().any(|existing| existing == program) {
            programs.push(program.clone());
        }
    }
}

fn write_record(record: &JsonlRecord<'_>) -> Result<()> {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, record)?;
    writeln!(stdout)?;
    Ok(())
}

/// The tail of a failing workload's stderr, folded onto one line for the
/// failure record.
///
/// The whole of both streams goes to the run log, but a lane keeps the
/// JSONL, and a cell whose `error` says nothing beyond "exited with code
/// 1" cost run 33959252438 a diagnosis it could have carried itself
/// (#150). An empty stream is said to be empty rather than left out.
fn quoted_stderr(stderr: &[u8]) -> String {
    /// Enough for a usage line, a `perror` line or a Rust `Error:` line;
    /// the run log holds the rest.
    const KEEP_CHARS: usize = 400;
    let text = String::from_utf8_lossy(stderr);
    let folded = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.is_empty() {
        return "; its stderr was empty".to_owned();
    }
    let count = folded.chars().count();
    if count <= KEEP_CHARS {
        return format!("; stderr: {folded}");
    }
    let start = folded
        .char_indices()
        .nth(count - KEEP_CHARS)
        .map(|(index, _)| index)
        .expect("the tail starts inside the folded stderr");
    format!("; stderr (last {KEEP_CHARS} chars): …{}", &folded[start..])
}

/// Prints a failing workload's own output, named and labelled.
///
/// A workload that exits non-zero used to surface as nothing but its
/// exit status, and the bytes it wrote went out unattributed — the
/// guest's stdout into the JSONL record stream a lane parses, its
/// stderr beside it with nothing saying which workload wrote it or
/// which stream it came from. Both streams go to stderr here, which is
/// where a lane log collects them, and each one is named so the log
/// says what the workload saw rather than only that it failed.
fn write_guest_output(workload: &Workload, stdout: &[u8], stderr: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut sink = std::io::stderr().lock();
    writeln!(sink, "--- workload {} output ---", workload.name)?;
    write_guest_stream(&mut sink, "stdout", stdout)?;
    write_guest_stream(&mut sink, "stderr", stderr)?;
    writeln!(sink, "--- end of workload {} output ---", workload.name)?;
    Ok(())
}

/// Prints the guest's per-shard network counters after a failed
/// workload iteration.
///
/// The lane keeps no guest tracing during a transfer and the guest logs
/// nothing at INFO mid-stream, so a stalled network workload used to
/// leave the log with the failure and nothing about the stack that
/// produced it: whether the receiver's window had shut, whether the
/// window update that should have reopened it was ever sent, whether
/// frames were being refused and on which shard (#143). One sample of
/// these answers all of that, and it is the last thing worth asking a
/// guest that has just failed.
///
/// Best effort by design. The iteration may have failed *because* the
/// guest stopped answering, and this must not turn a named failure into
/// a hang, so it is bounded like every other guest step and a guest that
/// says nothing is reported as saying nothing.
async fn write_guest_network_counters(
    client: &mut crate::serial::RpcClient,
    workload: &Workload,
    iteration: u16,
) {
    use std::io::Write as _;
    let sample = guest_step_under_deadline(
        "network counters",
        NETWORK_COUNTER_DEADLINE_SECONDS,
        crate::system::fetch_stats(client),
    )
    .await;
    let mut sink = std::io::stderr().lock();
    let _ = writeln!(
        sink,
        "--- workload {} iteration {iteration} network counters ---",
        workload.name
    );
    match sample {
        Ok(sample) => match sample.network {
            Some(network) if !network.queues.is_empty() => {
                for queue in network.queues {
                    let _ = writeln!(
                        sink,
                        "shard {} rx={} tx={} irq={} refused={} sockets={} shut-sockets={} \
                         window-bytes={} acks={} window-updates={} retransmits={}",
                        queue.id,
                        queue.rx_frames,
                        queue.tx_frames,
                        queue.interrupts,
                        queue.rx_refused_frames,
                        queue.tcp_sockets,
                        queue.tcp_receive_backpressured_sockets,
                        queue.tcp_receive_window_bytes,
                        queue.tcp_acks_sent,
                        queue.tcp_window_updates_sent,
                        queue.tcp_retransmits_sent,
                    );
                }
            }
            Some(_) | None => {
                let _ = writeln!(sink, "the guest reports no network device");
            }
        },
        Err(error) => {
            let _ = writeln!(sink, "the guest did not answer: {error}");
        }
    }
    let _ = writeln!(
        sink,
        "--- end of workload {} iteration {iteration} network counters ---",
        workload.name
    );
}

/// One named stream of a failing workload's output. An empty stream is
/// said to be empty rather than left out: "the guest printed nothing"
/// and "the runner did not capture this" are different failures.
fn write_guest_stream(sink: &mut impl std::io::Write, name: &str, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        writeln!(sink, "{name}: <empty>")?;
        return Ok(());
    }
    writeln!(sink, "{name} ({} bytes):", bytes.len())?;
    sink.write_all(bytes)?;
    if !bytes.ends_with(b"\n") {
        writeln!(sink)?;
    }
    Ok(())
}

fn git_sha() -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .context("failed to spawn git rev-parse HEAD")?;
    if !output.status.success() {
        bail!("git rev-parse HEAD exited with status {}", output.status);
    }
    Ok(String::from_utf8(output.stdout)
        .context("git rev-parse HEAD output was not UTF-8")?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed cell keeps only its `error` string, so the message has to
    /// carry the guest's own complaint (#150).
    #[test]
    fn a_failed_cell_quotes_the_workload_stderr() {
        assert_eq!(quoted_stderr(b""), "; its stderr was empty");
        assert_eq!(
            quoted_stderr(b"Error: tcp connect failed for 10.77.0.1:5001\n"),
            "; stderr: Error: tcp connect failed for 10.77.0.1:5001"
        );
        let long = "x".repeat(500);
        let quoted = quoted_stderr(long.as_bytes());
        assert!(
            quoted.starts_with("; stderr (last 400 chars): \u{2026}"),
            "the tail must say it is a tail, got {quoted}"
        );
        assert!(quoted.ends_with(&"x".repeat(400)));
    }

    #[test]
    fn manifest_contains_expected_workload_classes() {
        let command = WorkloadBenchCommand {
            manifest: Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("inspector crate must live under repo root")
                .join("tools/wasi-apps/workloads.json"),
            iterations: 5,
            workloads: Vec::new(),
            classes: Vec::new(),
            host_http_url: None,
            host_tcp_host: None,
            host_tcp_port: None,
            host_tcp_echo_port: None,
            keep_going: false,
            profile_output: None,
            kernel_profile_output: None,
            user_profile_output: None,
            perf_metrics_output: None,
            workload_timeout_seconds: DEFAULT_WORKLOAD_TIMEOUT_SECONDS,
        };
        let workloads = select_workloads(&command).expect("manifest must parse");
        assert!(
            workloads
                .iter()
                .any(|workload| workload.name == "quickjs-loop")
        );
        assert!(
            workloads
                .iter()
                .any(|workload| workload.name == "curl-local-http")
        );
        assert!(
            workloads
                .iter()
                .any(|workload| workload.runner == WorkloadRunner::HeliosAot)
        );
        let throughput = workloads
            .iter()
            .find(|workload| workload.name == "curl-http-throughput")
            .expect("manifest must contain HTTP throughput workload");
        assert_eq!(throughput.throughput_bytes, Some(67_108_864));
        let tcp_throughput = workloads
            .iter()
            .find(|workload| workload.name == "tcp-throughput")
            .expect("manifest must contain raw TCP throughput workload");
        assert!(tcp_throughput.requires_host_tcp);
        assert_eq!(tcp_throughput.throughput_bytes, Some(67_108_864));
        let wasi_tcp_throughput = workloads
            .iter()
            .find(|workload| workload.name == "wasi-tcp-throughput")
            .expect("manifest must contain WASI TCP throughput workload");
        assert!(wasi_tcp_throughput.requires_host_tcp);
        assert_eq!(wasi_tcp_throughput.throughput_bytes, Some(67_108_864));
        let wasix_tcp_throughput = workloads
            .iter()
            .find(|workload| workload.name == "wasix-tcp-throughput")
            .expect("manifest must contain WASIX TCP throughput workload");
        assert!(wasix_tcp_throughput.requires_host_tcp);
        assert_eq!(wasix_tcp_throughput.throughput_bytes, Some(67_108_864));
    }

    #[test]
    fn net_class_filter_excludes_compute_workloads() {
        let command = WorkloadBenchCommand {
            manifest: Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("inspector crate must live under repo root")
                .join("tools/wasi-apps/workloads.json"),
            iterations: 5,
            workloads: Vec::new(),
            classes: vec![WorkloadClass::Net],
            host_http_url: None,
            host_tcp_host: None,
            host_tcp_port: None,
            host_tcp_echo_port: None,
            keep_going: false,
            profile_output: None,
            kernel_profile_output: None,
            user_profile_output: None,
            perf_metrics_output: None,
            workload_timeout_seconds: DEFAULT_WORKLOAD_TIMEOUT_SECONDS,
        };
        let workloads = select_workloads(&command).expect("manifest must parse");
        assert!(
            workloads
                .iter()
                .all(|workload| workload.class == WorkloadClass::Net)
        );
    }

    #[test]
    fn headline_workloads_cover_every_design_claim() {
        let command = WorkloadBenchCommand {
            manifest: Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("inspector crate must live under repo root")
                .join("tools/wasi-apps/workloads.json"),
            iterations: 5,
            workloads: Vec::new(),
            classes: Vec::new(),
            host_http_url: None,
            host_tcp_host: None,
            host_tcp_port: None,
            host_tcp_echo_port: None,
            keep_going: false,
            profile_output: None,
            kernel_profile_output: None,
            user_profile_output: None,
            perf_metrics_output: None,
            workload_timeout_seconds: DEFAULT_WORKLOAD_TIMEOUT_SECONDS,
        };
        let workloads = select_workloads(&command).expect("manifest must parse");
        for class in [
            WorkloadClass::Startup,
            WorkloadClass::Hostcall,
            WorkloadClass::Ipc,
            WorkloadClass::Sched,
            WorkloadClass::Net,
            WorkloadClass::Fs,
            WorkloadClass::Compute,
        ] {
            assert!(
                workloads
                    .iter()
                    .any(|workload| workload.class == class && workload.headline),
                "class {class:?} has no headline workload"
            );
        }
        let latency = workloads
            .iter()
            .find(|workload| workload.name == "tcp-latency")
            .expect("manifest must contain the TCP latency workload");
        assert!(latency.requires_host_tcp_echo);
    }

    #[test]
    fn metric_lines_are_collected_and_validated() {
        let metrics =
            parse_metrics(b"pipe-pingpong:20000\nbench.rtt_p50_us=12.5\nbench.rtt_p99_us=40\n")
                .expect("well-formed metrics parse");
        assert_eq!(metrics.get("rtt_p50_us"), Some(&12.5));
        assert_eq!(metrics.get("rtt_p99_us"), Some(&40.0));
        assert!(parse_metrics(b"bench.broken\n").is_err());
        assert!(parse_metrics(b"bench.x=1\nbench.x=2\n").is_err());
    }

    #[test]
    fn throughput_rate_uses_manifest_payload_bytes() {
        assert_eq!(
            throughput_mib_per_second(Some(64 * 1024 * 1024), 64),
            Some(1000.0)
        );
        assert_eq!(throughput_mib_per_second(None, 64), None);
        assert_eq!(throughput_mib_per_second(Some(64 * 1024 * 1024), 0), None);
    }

    fn timeout_test_command(workload_timeout_seconds: u32) -> WorkloadBenchCommand {
        WorkloadBenchCommand {
            manifest: PathBuf::from("tools/wasi-apps/workloads.json"),
            iterations: 5,
            workloads: Vec::new(),
            classes: Vec::new(),
            host_http_url: None,
            host_tcp_host: None,
            host_tcp_port: None,
            host_tcp_echo_port: None,
            keep_going: false,
            profile_output: None,
            kernel_profile_output: None,
            user_profile_output: None,
            perf_metrics_output: None,
            workload_timeout_seconds,
        }
    }

    fn timeout_test_workload(name: &str) -> Workload {
        Workload {
            name: name.to_owned(),
            class: WorkloadClass::Net,
            headline: false,
            runner: WorkloadRunner::Program,
            _description: String::new(),
            command: None,
            program: None,
            args: Vec::new(),
            boot_programs: Vec::new(),
            stdout_contains: Vec::new(),
            stderr_empty: false,
            requires_host_http: false,
            requires_host_tcp: false,
            requires_host_tcp_echo: false,
            wasm_path: None,
            remote_path: None,
            destination_path: None,
            throughput_bytes: None,
        }
    }

    /// A workload that never answers is failed by name, not waited on.
    /// The RPC that reports a workload simply never returns when the
    /// guest stops making progress, so without this the lane held its
    /// runner until the job's own timeout hours later.
    #[test]
    fn a_workload_that_never_answers_is_failed_by_name_and_iteration() {
        let command = timeout_test_command(1);
        let workload = timeout_test_workload("tcp-throughput");

        let error = crate::runtime::block_on(under_deadline(
            &workload,
            3,
            &command,
            std::future::pending(),
        ))
        .expect_err("a workload that never answers must fail rather than hang");

        let timed_out = error
            .downcast_ref::<WorkloadBenchError>()
            .expect("the failure must be the typed deadline error");
        assert!(
            matches!(
                timed_out,
                WorkloadBenchError::WorkloadTimedOut {
                    workload,
                    iteration: 3,
                    seconds: 1,
                } if workload == "tcp-throughput"
            ),
            "the failure must name the workload, the iteration and the deadline, got {timed_out}"
        );
    }

    /// A workload that answers inside the deadline is untouched by it.
    #[test]
    fn a_workload_that_answers_in_time_keeps_its_measurement() {
        let command = timeout_test_command(DEFAULT_WORKLOAD_TIMEOUT_SECONDS);
        let workload = timeout_test_workload("process-startup");

        let output = crate::runtime::block_on(under_deadline(
            &workload,
            1,
            &command,
            std::future::ready(Ok(WorkloadOutput {
                elapsed_ms: 24,
                stdout: b"process-startup:ok\n".to_vec(),
                stderr: Vec::new(),
            })),
        ))
        .expect("a workload that answers in time must not be failed");

        assert_eq!(output.elapsed_ms, 24);
    }

    /// The steps around the workloads are bounded too.
    ///
    /// The profile hand-off, the tracing fetch and the teardown all talk
    /// to the same guest as a workload does, and a guest that stops
    /// answering stops answering those as well. Run 33952047436 hung
    /// there for ninety-five minutes after its last workload was
    /// recorded, holding QEMU open behind it.
    #[test]
    fn a_guest_step_that_never_answers_is_failed_by_name() {
        let error = crate::runtime::block_on(guest_step_under_deadline(
            "the final profile read",
            1,
            std::future::pending::<Result<()>>(),
        ))
        .expect_err("a guest step that never answers must fail rather than hang");

        let timed_out = error
            .downcast_ref::<WorkloadBenchError>()
            .expect("the failure must be the typed deadline error");
        assert!(
            matches!(
                timed_out,
                WorkloadBenchError::GuestStepTimedOut {
                    step: "the final profile read",
                    seconds: 1,
                }
            ),
            "the failure must name the step and the deadline, got {timed_out}"
        );
    }
}
