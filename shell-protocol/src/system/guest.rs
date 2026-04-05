use std::io;

use anyhow::{Context as _, Result, bail};
use futures_io::{AsyncRead, AsyncWrite};
use helios_api::{
    instances as host_instances, programs as host_programs, stats as host_stats,
    tracing as host_tracing,
};

use crate::wire::{Frame, read_frame, write_frame};

use super::bindings::helios::system::{instances, programs, stats, tracing};
use crate::debugger::filesystem;

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
        write_frame(
            &mut write,
            &Frame::Data {
                invocation,
                path: Vec::new(),
                payload: response,
            },
        )
        .await
        .context("failed to write debugger response payload")?;
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
        ("helios:system/programs@0.1.0", "launch")
            | ("helios:system/stats@0.1.0", "snapshot")
            | ("helios:system/instances@0.1.0", "snapshot")
            | ("helios:system/tracing@0.1.0", "recent")
    ) || filesystem::supports(instance, func)
}

async fn dispatch(instance: &str, func: &str, payload: &[u8]) -> Result<Vec<u8>> {
    match (instance, func) {
        ("helios:system/programs@0.1.0", "launch") => {
            let request = postcard::from_bytes::<programs::LaunchRequest>(payload)
                .context("failed to decode programs.launch request payload")?;
            let response = host_programs::launch(&host_programs::LaunchRequest {
                name: request.name,
                args: request.args,
                wasm: request.wasm,
            });
            let response = response.map_err(|error| programs::LaunchError {
                kind: convert_launch_error_kind(error.kind),
                detail: error.detail,
            });
            postcard::to_allocvec(&response).context("failed to encode programs.launch response")
        }
        ("helios:system/stats@0.1.0", "snapshot") => {
            if !payload.is_empty() {
                bail!("stats.snapshot does not accept request payload bytes");
            }
            let snapshot = convert_sample(host_stats::snapshot());
            postcard::to_allocvec(&snapshot).context("failed to encode stats snapshot response")
        }
        ("helios:system/instances@0.1.0", "snapshot") => {
            if !payload.is_empty() {
                bail!("instances.snapshot does not accept request payload bytes");
            }
            let snapshot = host_instances::snapshot()
                .into_iter()
                .map(convert_instance)
                .collect::<Vec<_>>();
            postcard::to_allocvec(&snapshot).context("failed to encode instances snapshot response")
        }
        ("helios:system/tracing@0.1.0", "recent") => {
            let (filter, limit): (tracing::Filter, u32) = postcard::from_bytes(payload)
                .context("failed to decode tracing.recent request payload")?;
            let events = host_tracing::recent(&convert_filter(filter), limit)
                .into_iter()
                .map(convert_event)
                .collect::<Vec<_>>();
            postcard::to_allocvec(&events).context("failed to encode tracing.recent response")
        }
        _ if filesystem::supports(instance, func) => filesystem::dispatch(func, payload).await,
        _ => unreachable!("supports_request must reject unsupported methods before dispatch"),
    }
}

fn convert_launch_error_kind(kind: host_programs::LaunchErrorKind) -> programs::LaunchErrorKind {
    match kind {
        host_programs::LaunchErrorKind::InvalidBinary => programs::LaunchErrorKind::InvalidBinary,
        host_programs::LaunchErrorKind::MissingEntry => programs::LaunchErrorKind::MissingEntry,
        host_programs::LaunchErrorKind::UnsupportedImport => {
            programs::LaunchErrorKind::UnsupportedImport
        }
        host_programs::LaunchErrorKind::QueueSaturated => programs::LaunchErrorKind::QueueSaturated,
        host_programs::LaunchErrorKind::Unavailable => programs::LaunchErrorKind::Unavailable,
        host_programs::LaunchErrorKind::Internal => programs::LaunchErrorKind::Internal,
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
