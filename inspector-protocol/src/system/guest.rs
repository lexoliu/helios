use std::io;

use anyhow::{Context as _, Result, bail};
use futures_io::{AsyncRead, AsyncWrite};
use helios_api::{
    instances as host_instances, profiling as host_profiling, programs as host_programs,
    stats as host_stats, tracing as host_tracing,
};

use crate::wire::{Frame, read_frame, write_frame};

use super::bindings::helios::system::{instances, profiling, programs, stats, tracing};
use super::methods::{
    INSTANCES_INSTANCE, INSTANCES_SNAPSHOT, PROFILING_CLEAR, PROFILING_FOLDED, PROFILING_INSTANCE,
    PROFILING_SET_ENABLED, PROGRAMS_AOT, PROGRAMS_EXEC, PROGRAMS_INSTANCE, STATS_INSTANCE,
    STATS_SNAPSHOT, TRACING_INSTANCE, TRACING_RECENT,
};
use crate::debugger::{filesystem, programs as debugger_programs};

const RESPONSE_CHUNK_BYTES: usize = 64 * 1024;

pub async fn serve<R, W>(mut read: R, mut write: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let Some(Frame::Open {
            invocation,
            instance,
            func,
        }) = read_frame(&mut read)
            .await
            .context("failed to read debugger request frame")?
        else {
            return Ok(());
        };

        if !supports_request(&instance, &func) {
            write_frame(
                &mut write,
                &Frame::Reject {
                    invocation,
                    message: format!(
                        "remote invocation {instance}.{func} is not exposed by the embedded debugger"
                    ),
                },
            )
            .await
            .context("failed to reject unsupported debugger request")?;
            continue;
        }

        write_frame(&mut write, &Frame::Accept { invocation })
            .await
            .context("failed to accept debugger request stream")?;
        let payload = read_root_payload(&mut read, invocation).await?;
        let response = match dispatch(&instance, &func, &payload).await {
            Ok(response) => response,
            Err(error) => {
                write_frame(
                    &mut write,
                    &Frame::Reject {
                        invocation,
                        message: format!("{error:#}"),
                    },
                )
                .await
                .context("failed to report debugger request failure")?;
                continue;
            }
        };
        for chunk in response.chunks(RESPONSE_CHUNK_BYTES) {
            write_frame(
                &mut write,
                &Frame::Data {
                    invocation,
                    path: Vec::new(),
                    payload: chunk.to_vec(),
                },
            )
            .await
            .context("failed to write debugger response payload")?;
        }
        write_frame(
            &mut write,
            &Frame::Close {
                invocation,
                path: Vec::new(),
            },
        )
        .await
        .context("failed to close debugger response stream")?;
    }
}

fn supports_request(instance: &str, func: &str) -> bool {
    matches!(
        (instance, func),
        (PROGRAMS_INSTANCE, PROGRAMS_EXEC)
            | (PROGRAMS_INSTANCE, PROGRAMS_AOT)
            | (STATS_INSTANCE, STATS_SNAPSHOT)
            | (INSTANCES_INSTANCE, INSTANCES_SNAPSHOT)
            | (TRACING_INSTANCE, TRACING_RECENT)
            | (PROFILING_INSTANCE, PROFILING_SET_ENABLED)
            | (PROFILING_INSTANCE, PROFILING_CLEAR)
            | (PROFILING_INSTANCE, PROFILING_FOLDED)
    ) || filesystem::supports(instance, func)
        || debugger_programs::supports(instance, func)
}

async fn dispatch(instance: &str, func: &str, payload: &[u8]) -> Result<Vec<u8>> {
    match (instance, func) {
        (PROGRAMS_INSTANCE, PROGRAMS_EXEC) => {
            let request = postcard::from_bytes::<programs::ExecRequest>(payload)
                .context("failed to decode programs.exec request payload")?;
            let response = host_programs::exec(host_programs::ExecRequest {
                name: request.name,
                args: request.args,
                env: Vec::new(),
                path: request.path,
                stdin: Vec::new(),
                hint: request.hint.map(map_aot_hint_to_host),
            })
            .await;
            let response = response
                .map(|result| programs::ExecResult {
                    instance_id: result.instance_id,
                    exit_code: result.exit_code,
                    output: programs::ExecOutput {
                        stdout: result.output.stdout,
                        stderr: result.output.stderr,
                    },
                })
                .map_err(|error| programs::ExecError {
                    kind: convert_launch_error_kind(error.kind),
                    detail: error.detail,
                });
            postcard::to_allocvec(&response).context("failed to encode programs.exec response")
        }
        (PROGRAMS_INSTANCE, PROGRAMS_AOT) => {
            let request = postcard::from_bytes::<programs::AotRequest>(payload)
                .context("failed to decode programs.aot request payload")?;
            let response = host_programs::aot(host_programs::AotRequest {
                source_path: request.source_path,
                destination_path: request.destination_path,
                hint: map_aot_hint_to_host(request.hint),
                profile: request.profile,
            })
            .await;
            let response = response
                .map(|result| programs::AotResult {
                    destination_path: result.destination_path,
                })
                .map_err(|error| programs::ExecError {
                    kind: convert_launch_error_kind(error.kind),
                    detail: error.detail,
                });
            postcard::to_allocvec(&response).context("failed to encode programs.aot response")
        }
        (STATS_INSTANCE, STATS_SNAPSHOT) => {
            if !payload.is_empty() {
                bail!("stats.snapshot does not accept request payload bytes");
            }
            let snapshot = convert_sample(host_stats::snapshot());
            postcard::to_allocvec(&snapshot).context("failed to encode stats snapshot response")
        }
        (INSTANCES_INSTANCE, INSTANCES_SNAPSHOT) => {
            if !payload.is_empty() {
                bail!("instances.snapshot does not accept request payload bytes");
            }
            let snapshot = host_instances::snapshot()
                .into_iter()
                .map(convert_instance)
                .collect::<Vec<_>>();
            postcard::to_allocvec(&snapshot).context("failed to encode instances snapshot response")
        }
        (TRACING_INSTANCE, TRACING_RECENT) => {
            let (filter, limit): (tracing::Filter, u32) = postcard::from_bytes(payload)
                .context("failed to decode tracing.recent request payload")?;
            let events = host_tracing::recent(&convert_filter(filter), limit)
                .into_iter()
                .map(convert_event)
                .collect::<Vec<_>>();
            postcard::to_allocvec(&events).context("failed to encode tracing.recent response")
        }
        (PROFILING_INSTANCE, PROFILING_SET_ENABLED) => {
            let enabled = postcard::from_bytes::<bool>(payload)
                .context("failed to decode profiling.set-enabled request payload")?;
            host_profiling::set_enabled(enabled);
            postcard::to_allocvec(&()).context("failed to encode profiling.set-enabled response")
        }
        (PROFILING_INSTANCE, PROFILING_CLEAR) => {
            if !payload.is_empty() {
                bail!("profiling.clear does not accept request payload bytes");
            }
            host_profiling::clear();
            postcard::to_allocvec(&()).context("failed to encode profiling.clear response")
        }
        (PROFILING_INSTANCE, PROFILING_FOLDED) => {
            let (filter, limit): (profiling::Filter, u32) = postcard::from_bytes(payload)
                .context("failed to decode profiling.folded request payload")?;
            let samples = host_profiling::folded(&convert_profile_filter(filter), limit)
                .into_iter()
                .map(convert_profile_sample)
                .collect::<Vec<_>>();
            postcard::to_allocvec(&samples).context("failed to encode profiling.folded response")
        }
        _ if filesystem::supports(instance, func) => filesystem::dispatch(func, payload).await,
        _ if debugger_programs::supports(instance, func) => {
            debugger_programs::dispatch(func, payload).await
        }
        _ => unreachable!("supports_request must reject unsupported methods before dispatch"),
    }
}

fn map_aot_hint_to_host(hint: programs::AotHint) -> host_programs::AotHint {
    match hint {
        programs::AotHint::Fast => host_programs::AotHint::Fast,
        programs::AotHint::Balanced => host_programs::AotHint::Balanced,
        programs::AotHint::Performance => host_programs::AotHint::Performance,
    }
}

fn convert_launch_error_kind(kind: host_programs::ExecErrorKind) -> programs::ExecErrorKind {
    match kind {
        host_programs::ExecErrorKind::InvalidBinary => programs::ExecErrorKind::InvalidBinary,
        host_programs::ExecErrorKind::MissingEntry => programs::ExecErrorKind::MissingEntry,
        host_programs::ExecErrorKind::UnsupportedImport => {
            programs::ExecErrorKind::UnsupportedImport
        }
        host_programs::ExecErrorKind::InvalidSignature => programs::ExecErrorKind::InvalidSignature,
        host_programs::ExecErrorKind::InvalidPath => programs::ExecErrorKind::InvalidPath,
        host_programs::ExecErrorKind::InvalidHint => programs::ExecErrorKind::InvalidHint,
        host_programs::ExecErrorKind::OutOfMemory => programs::ExecErrorKind::OutOfMemory,
        host_programs::ExecErrorKind::Unavailable => programs::ExecErrorKind::Unavailable,
        host_programs::ExecErrorKind::Internal => programs::ExecErrorKind::Internal,
    }
}

async fn read_root_payload<R>(read: &mut R, invocation: u32) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut payload = Vec::new();
    loop {
        match read_frame(read)
            .await
            .context("failed to read request payload frame")?
        {
            Some(Frame::Data {
                invocation: frame_invocation,
                path,
                payload: chunk,
            }) => {
                if frame_invocation != invocation {
                    bail!(
                        "received payload for invocation {} while reading {}",
                        frame_invocation,
                        invocation
                    );
                }
                if !path.is_empty() {
                    bail!("nested request stream paths are unsupported in the guest debugger");
                }
                payload.extend_from_slice(&chunk);
            }
            Some(Frame::Close {
                invocation: frame_invocation,
                path,
            }) => {
                if frame_invocation != invocation {
                    bail!(
                        "received close for invocation {} while reading {}",
                        frame_invocation,
                        invocation
                    );
                }
                if !path.is_empty() {
                    bail!("nested request stream paths are unsupported in the guest debugger");
                }
                return Ok(payload);
            }
            Some(Frame::Reject { .. } | Frame::Accept { .. } | Frame::Open { .. }) => {
                bail!("unexpected control frame while reading debugger request payload");
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "transport closed while reading debugger request payload",
                )
                .into());
            }
        }
    }
}

fn convert_sample(sample: host_stats::Sample) -> stats::Sample {
    stats::Sample {
        timestamp: sample.timestamp,
        uptime: sample.uptime,
        processors: stats::Processors {
            configured: sample.processors.configured,
            online: sample.processors.online,
            utilization: sample
                .processors
                .utilization
                .into_iter()
                .map(convert_processor)
                .collect(),
        },
        memory: stats::Memory {
            total_bytes: sample.memory.total_bytes,
            available_bytes: sample.memory.available_bytes,
            pressure: convert_memory_pressure(sample.memory.pressure),
        },
    }
}

fn convert_processor(processor: host_stats::Processor) -> stats::Processor {
    stats::Processor {
        id: processor.id,
        busy: processor.busy,
    }
}

fn convert_memory_pressure(pressure: host_stats::MemoryPressure) -> stats::MemoryPressure {
    match pressure {
        host_stats::MemoryPressure::Nominal => stats::MemoryPressure::Nominal,
        host_stats::MemoryPressure::Elevated => stats::MemoryPressure::Elevated,
        host_stats::MemoryPressure::High => stats::MemoryPressure::High,
        host_stats::MemoryPressure::Critical => stats::MemoryPressure::Critical,
    }
}

fn convert_instance(instance: host_instances::Instance) -> instances::Instance {
    instances::Instance {
        id: instance.id,
        name: instance.name,
        started_at: instance.started_at,
        uptime: instance.uptime,
        memory_bytes: instance.memory_bytes,
        cpu_busy: instance.cpu_busy,
    }
}

fn convert_filter(filter: tracing::Filter) -> host_tracing::Filter {
    host_tracing::Filter {
        min_level: filter.min_level.map(convert_level_to_local),
        target_prefixes: filter.target_prefixes,
    }
}

fn convert_profile_filter(filter: profiling::Filter) -> host_profiling::Filter {
    host_profiling::Filter {
        scope: filter.scope.map(convert_profile_scope_to_local),
        stack_prefixes: filter.stack_prefixes,
    }
}

fn convert_profile_sample(sample: host_profiling::FoldedSample) -> profiling::FoldedSample {
    profiling::FoldedSample {
        scope: convert_profile_scope_from_local(sample.scope),
        stack: sample.stack,
        weight: sample.weight,
    }
}

fn convert_event(event: host_tracing::Event) -> tracing::Event {
    tracing::Event {
        timestamp: event.timestamp,
        level: convert_level_from_local(event.level),
        target: event.target,
        fields: event.fields.into_iter().map(convert_field).collect(),
    }
}

fn convert_field(field: host_tracing::Field) -> tracing::Field {
    tracing::Field {
        key: field.key,
        value: convert_value(field.value),
    }
}

fn convert_value(value: host_tracing::Value) -> tracing::Value {
    match value {
        host_tracing::Value::Boolean(value) => tracing::Value::Boolean(value),
        host_tracing::Value::Signed64(value) => tracing::Value::Signed64(value),
        host_tracing::Value::Unsigned64(value) => tracing::Value::Unsigned64(value),
        host_tracing::Value::Float64(value) => tracing::Value::Float64(value),
        host_tracing::Value::Text(value) => tracing::Value::Text(value),
        host_tracing::Value::Blob(value) => tracing::Value::Blob(value),
    }
}

fn convert_profile_scope_from_local(scope: host_profiling::Scope) -> profiling::Scope {
    match scope {
        host_profiling::Scope::Kernel => profiling::Scope::Kernel,
        host_profiling::Scope::User => profiling::Scope::User,
    }
}

fn convert_profile_scope_to_local(scope: profiling::Scope) -> host_profiling::Scope {
    match scope {
        profiling::Scope::Kernel => host_profiling::Scope::Kernel,
        profiling::Scope::User => host_profiling::Scope::User,
    }
}

fn convert_level_from_local(level: host_tracing::Level) -> tracing::Level {
    match level {
        host_tracing::Level::Error => tracing::Level::Error,
        host_tracing::Level::Warn => tracing::Level::Warn,
        host_tracing::Level::Info => tracing::Level::Info,
        host_tracing::Level::Debug => tracing::Level::Debug,
        host_tracing::Level::Trace => tracing::Level::Trace,
    }
}

fn convert_level_to_local(level: tracing::Level) -> host_tracing::Level {
    match level {
        tracing::Level::Error => host_tracing::Level::Error,
        tracing::Level::Warn => host_tracing::Level::Warn,
        tracing::Level::Info => host_tracing::Level::Info,
        tracing::Level::Debug => host_tracing::Level::Debug,
        tracing::Level::Trace => host_tracing::Level::Trace,
    }
}
