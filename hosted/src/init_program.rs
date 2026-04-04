use std::fs;
use std::path::Path;
use std::thread;

use helios_kernel::{
    EmbeddedComponent, EmbeddedDebugger, EmbeddedInit, Kernel, embedded_debugger, embedded_init,
};
use tempfile::TempDir;
use thiserror::Error;
use wasmtime::component::{HasSelf, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::cli;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::config::HostedConfig;
use crate::cpu::HostedCpu;
use crate::observer_buffer::SharedObserverBuffer;
use crate::program_bindings::bindings::Init;
use crate::serial_host::HostedSerialIo;

const WASMTIME_TARGET: &str = env!("HELIOS_HOSTED_TARGET");

pub fn spawn_init(
    _kernel: &Kernel<HostedCpu>,
    config: &HostedConfig,
    observer: SharedObserverBuffer,
) {
    let init = embedded_init()
        .unwrap_or_else(|| panic!("no embedded init program found; set HELIOS_INIT_WASM"));
    let config = config.clone();

    thread::Builder::new()
        .name("helios-init".to_owned())
        .spawn(move || run_component_thread(EmbeddedHostedProgram::Init(init), config, observer))
        .unwrap_or_else(|error| panic!("failed to spawn hosted init thread: {error}"));
}

pub fn spawn_debugger(
    _kernel: &Kernel<HostedCpu>,
    config: &HostedConfig,
    observer: SharedObserverBuffer,
) {
    let debugger = embedded_debugger()
        .unwrap_or_else(|| panic!("no embedded debugger program found; set HELIOS_DEBUGGER_WASM"));
    let config = config.clone();

    thread::Builder::new()
        .name("helios-debugger".to_owned())
        .spawn(move || {
            run_component_thread(EmbeddedHostedProgram::Debugger(debugger), config, observer)
        })
        .unwrap_or_else(|error| panic!("failed to spawn hosted debugger thread: {error}"));
}

fn run_component_thread(
    program: EmbeddedHostedProgram,
    config: HostedConfig,
    observer: SharedObserverBuffer,
) {
    run_component_thread_with_serial(program, config, observer, Some(HostedSerialIo::new()))
        .unwrap_or_else(|error| panic!("failed to launch embedded component: {error}"));
}

fn run_component_thread_with_serial(
    program: EmbeddedHostedProgram,
    config: HostedConfig,
    observer: SharedObserverBuffer,
    serial: Option<HostedSerialIo>,
) -> Result<(), HostedProgramError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("failed to build hosted component runtime: {error}"));

    runtime.block_on(run_component_with_serial(
        program, &config, observer, serial,
    ))
}

async fn run_component_with_serial(
    program: EmbeddedHostedProgram,
    config: &HostedConfig,
    observer: SharedObserverBuffer,
    serial: Option<HostedSerialIo>,
) -> Result<(), HostedProgramError> {
    let engine = build_engine()?;
    let component =
        wasmtime::component::Component::from_binary(&engine, program.component().bytes())
            .map_err(HostedProgramError::CompileComponent)?;

    let mut linker = wasmtime::component::Linker::<StoreData>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(HostedProgramError::LinkComponent)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker).map_err(HostedProgramError::LinkComponent)?;
    crate::program_bindings::bindings::helios::system::serial::add_to_linker::<_, HasSelf<_>>(
        &mut linker,
        |state| state,
    )
    .map_err(HostedProgramError::LinkComponent)?;
    crate::program_bindings::bindings::helios::system::sync::add_to_linker::<_, HasSelf<_>>(
        &mut linker,
        |state| state,
    )
    .map_err(HostedProgramError::LinkComponent)?;
    crate::observer_host::add_to_linker(&mut linker).map_err(HostedProgramError::LinkComponent)?;

    let mounted_bootfs = match &program {
        EmbeddedHostedProgram::Init(init) => {
            Some(mount_bootfs(init).map_err(HostedProgramError::MountBootfs)?)
        }
        EmbeddedHostedProgram::Debugger(_) => None,
    };
    let wasi = build_wasi_ctx(config, &program, mounted_bootfs.as_ref())
        .map_err(HostedProgramError::ConfigureWasi)?;
    let mut store = Store::new(
        &engine,
        StoreData {
            table: ResourceTable::new(),
            wasi,
            serial,
            processor_count: config.processor_count() as u32,
            started_at: std::time::Instant::now(),
            observer,
            mounted_bootfs,
        },
    );

    let instance = Init::instantiate_async(&mut store, &component, &linker)
        .await
        .map_err(HostedProgramError::InstantiateComponent)?;
    let result = store
        .run_concurrent(async move |accessor| instance.wasi_cli_run().call_run(accessor).await)
        .await
        .map_err(HostedProgramError::RunConcurrent)?
        .map_err(HostedProgramError::RunComponent)?;
    result.map_err(|()| HostedProgramError::GuestFailed(program.name().to_owned()))?;
    Ok(())
}

fn build_engine() -> Result<Engine, HostedProgramError> {
    let mut config = Config::new();
    config
        .target(WASMTIME_TARGET)
        .expect("hosted Wasmtime target must be accepted");
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config).map_err(HostedProgramError::CreateEngine)
}

fn build_wasi_ctx(
    config: &HostedConfig,
    program: &EmbeddedHostedProgram,
    mounted_bootfs: Option<&MountedBootFs>,
) -> Result<WasiCtx, wasmtime::Error> {
    let mut builder = WasiCtxBuilder::new();
    builder.arg(program.argv0());
    builder.stdout(cli::stderr());
    builder.stderr(cli::stderr());

    if matches!(program, EmbeddedHostedProgram::Init(_)) {
        if !config.init_args().is_empty() {
            builder.args(config.init_args());
        }
        if !config.init_env().is_empty() {
            builder.envs(config.init_env());
        }
        if let Some(root) = config.init_wasi_root() {
            builder.preopened_dir(root, "/", DirPerms::READ, FilePerms::READ)?;
        }
        if let Some(bootfs) = mounted_bootfs {
            builder.preopened_dir(bootfs.path(), "/", DirPerms::READ, FilePerms::READ)?;
        }
    }

    Ok(builder.build())
}

fn mount_bootfs(init: &EmbeddedInit) -> Result<MountedBootFs, BootFsMountError> {
    let directory = tempfile::tempdir().map_err(BootFsMountError::CreateDirectory)?;
    for file in init.bootfs().files() {
        let path = directory.path().join(file.path());
        let parent = path.parent().unwrap_or_else(|| {
            panic!(
                "embedded bootfs file {} has no parent directory",
                file.path()
            )
        });
        fs::create_dir_all(parent).map_err(|error| BootFsMountError::CreatePath {
            path: parent.to_owned(),
            source: error,
        })?;
        fs::write(&path, file.contents()).map_err(|error| BootFsMountError::WriteFile {
            path: path.clone(),
            source: error,
        })?;
        let mut permissions = fs::metadata(&path)
            .map_err(|error| BootFsMountError::ReadMetadata {
                path: path.clone(),
                source: error,
            })?
            .permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).map_err(|error| {
            BootFsMountError::SetPermissions {
                path,
                source: error,
            }
        })?;
    }
    Ok(MountedBootFs { directory })
}

pub(crate) struct StoreData {
    pub(crate) table: ResourceTable,
    pub(crate) wasi: WasiCtx,
    pub(crate) serial: Option<HostedSerialIo>,
    pub(crate) processor_count: u32,
    pub(crate) started_at: std::time::Instant,
    pub(crate) observer: SharedObserverBuffer,
    mounted_bootfs: Option<MountedBootFs>,
}

impl WasiView for StoreData {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        let _ = self.mounted_bootfs.as_ref();
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

enum EmbeddedHostedProgram {
    Init(EmbeddedInit),
    Debugger(EmbeddedDebugger),
}

impl EmbeddedHostedProgram {
    fn component(&self) -> EmbeddedComponent {
        match self {
            Self::Init(init) => init.component(),
            Self::Debugger(debugger) => debugger.component(),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Init(_) => "init",
            Self::Debugger(_) => "debugger",
        }
    }

    fn argv0(&self) -> &str {
        match self {
            Self::Init(init) => init.argv0(),
            Self::Debugger(_) => "helios-debugger",
        }
    }
}

struct MountedBootFs {
    directory: TempDir,
}

impl MountedBootFs {
    fn path(&self) -> &Path {
        self.directory.path()
    }
}

#[derive(Debug, Error)]
enum BootFsMountError {
    #[error("failed to create hosted bootfs mount directory: {0}")]
    CreateDirectory(std::io::Error),
    #[error("failed to create hosted bootfs directory {path}: {source}")]
    CreatePath {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write hosted bootfs file {path}: {source}")]
    WriteFile {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read metadata for hosted bootfs file {path}: {source}")]
    ReadMetadata {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("failed to make hosted bootfs file read-only {path}: {source}")]
    SetPermissions {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Error)]
enum HostedProgramError {
    #[error("failed to initialize Wasmtime engine: {0}")]
    CreateEngine(wasmtime::Error),
    #[error("failed to configure WASI context: {0}")]
    ConfigureWasi(wasmtime::Error),
    #[error("failed to materialize embedded bootfs: {0}")]
    MountBootfs(BootFsMountError),
    #[error("failed to JIT-compile embedded component: {0}")]
    CompileComponent(wasmtime::Error),
    #[error("failed to add host bindings: {0}")]
    LinkComponent(wasmtime::Error),
    #[error("failed to instantiate component: {0}")]
    InstantiateComponent(wasmtime::Error),
    #[error("failed to drive concurrent component execution: {0}")]
    RunConcurrent(wasmtime::Error),
    #[error("component trapped: {0}")]
    RunComponent(wasmtime::Error),
    #[error("{0} component returned a non-zero result")]
    GuestFailed(String),
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;
    use std::time::Instant as StdInstant;

    use helios_kernel::embedded_debugger;
    use tokio::time::timeout;
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use super::{EmbeddedHostedProgram, HostedConfig, run_component_thread_with_serial};
    use crate::observer_buffer::ObserverBuffer;
    use crate::serial_host::HostedSerialIo;

    fn format_serial_chunks(chunks: &[Vec<u8>]) -> String {
        chunks
            .iter()
            .map(|chunk| {
                chunk
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn embedded_debugger_serves_remote_stats_and_tracing_over_serial() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| {
                panic!("failed to build tokio runtime for hosted debugger e2e test: {error}")
            })
            .block_on(async {
                let debugger = embedded_debugger()
                    .unwrap_or_else(|| panic!("embedded debugger component is missing"));
                let config = HostedConfig::from_env();
                let observer = ObserverBuffer::new(StdInstant::now());
                observer.record_console_text("INFO [helios_kernel] Kernel initialized\n");

                let (serial, peer, serial_trace) = HostedSerialIo::traced_duplex_pair(4096);
                let component_config = config.clone();
                let component_observer = observer.clone();
                let (result_tx, result_rx) = mpsc::sync_channel(1);
                let component_thread = std::thread::spawn(move || {
                    let result = run_component_thread_with_serial(
                        EmbeddedHostedProgram::Debugger(debugger),
                        component_config,
                        component_observer,
                        Some(serial),
                    );
                    result_tx.send(result).unwrap_or_else(|error| {
                        panic!("failed to send debugger thread result: {error}")
                    });
                });

                let (peer_read, peer_write) = tokio::io::split(peer);
                let mut client = helios_shell_protocol::transport::Client::new(
                    peer_read.compat(),
                    peer_write.compat_write(),
                );
                tokio::task::yield_now().await;
                std::thread::sleep(Duration::from_millis(100));

                match result_rx.try_recv() {
                    Ok(result) => {
                        component_thread.join().unwrap_or_else(|_| {
                            panic!("hosted debugger thread panicked unexpectedly")
                        });
                        panic!("debugger thread exited before serving RPCs: {result:?}");
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        component_thread.join().unwrap_or_else(|_| {
                            panic!("hosted debugger thread panicked unexpectedly")
                        });
                        panic!("debugger thread disconnected before serving RPCs");
                    }
                }

                let stats = timeout(
                    Duration::from_secs(30),
                    helios_shell_protocol::system::stats::snapshot(&mut client),
                )
                .await
                    .unwrap_or_else(|_| {
                        panic!(
                            "timed out waiting for remote stats snapshot (reads={}, writes={}, flushes={}, read-bytes={}, write-bytes={})",
                            serial_trace.reads(),
                            serial_trace.writes(),
                            serial_trace.flushes(),
                            format_serial_chunks(&serial_trace.read_bytes()),
                            format_serial_chunks(&serial_trace.write_bytes()),
                        )
                    })
                    .unwrap_or_else(|error| {
                        let debugger_status = match result_rx.try_recv() {
                            Ok(result) => format!("thread exited with {result:?}"),
                            Err(mpsc::TryRecvError::Empty) => "thread still running".to_owned(),
                            Err(mpsc::TryRecvError::Disconnected) => {
                                "thread panicked or disconnected".to_owned()
                            }
                        };
                        panic!(
                            "failed to fetch remote stats: {error:#}; {debugger_status}; reads={}, writes={}, flushes={}, read-bytes={}, write-bytes={}",
                            serial_trace.reads(),
                            serial_trace.writes(),
                            serial_trace.flushes(),
                            format_serial_chunks(&serial_trace.read_bytes()),
                            format_serial_chunks(&serial_trace.write_bytes()),
                        )
                    });
                assert!(
                    stats.timestamp <= stats.uptime,
                    "stats timestamp must not exceed uptime"
                );
                assert_eq!(
                    stats.processors.utilization.len(),
                    stats.processors.online as usize,
                    "stats processor list must match reported online processor count"
                );

                let second_stats = timeout(
                    Duration::from_secs(30),
                    helios_shell_protocol::system::stats::snapshot(&mut client),
                )
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timed out waiting for second remote stats snapshot; reads={}, writes={}, flushes={}, read-bytes={}, write-bytes={}",
                        serial_trace.reads(),
                        serial_trace.writes(),
                        serial_trace.flushes(),
                        format_serial_chunks(&serial_trace.read_bytes()),
                        format_serial_chunks(&serial_trace.write_bytes()),
                    )
                })
                .unwrap_or_else(|error| panic!("failed to fetch second remote stats: {error:#}"));
                assert_eq!(
                    second_stats.processors.utilization.len(),
                    second_stats.processors.online as usize,
                    "second stats processor list must match reported online processor count"
                );

                let early_raw_mutex = timeout(
                    Duration::from_secs(30),
                    helios_shell_protocol::system::sync::raw_mutex(&mut client),
                )
                .await
                .unwrap_or_else(|_| panic!("timed out creating early remote raw mutex"))
                .unwrap_or_else(|error| panic!("failed to create early remote raw mutex: {error}"));
                let early_raw_mutex_guard =
                    timeout(Duration::from_secs(30), early_raw_mutex.lock(&mut client))
                        .await
                        .unwrap_or_else(|_| panic!("timed out locking early remote raw mutex"))
                        .unwrap_or_else(|error| {
                            panic!("failed to lock early remote raw mutex: {error}")
                        });
                timeout(
                    Duration::from_secs(30),
                    early_raw_mutex_guard.drop_remote(&mut client),
                )
                .await
                .unwrap_or_else(|_| panic!("timed out dropping early remote raw mutex guard"))
                .unwrap_or_else(|error| {
                    panic!("failed to drop early remote raw mutex guard: {error}")
                });
                timeout(Duration::from_secs(30), early_raw_mutex.drop_remote(&mut client))
                    .await
                    .unwrap_or_else(|_| panic!("timed out dropping early remote raw mutex"))
                    .unwrap_or_else(|error| {
                        panic!("failed to drop early remote raw mutex: {error}")
                    });

                let debug_port = timeout(
                    Duration::from_secs(30),
                    helios_shell_protocol::system::serial::debug_port(&mut client),
                )
                .await
                .unwrap_or_else(|_| {
                    let debugger_status = match result_rx.try_recv() {
                        Ok(result) => format!("thread exited with {result:?}"),
                        Err(mpsc::TryRecvError::Empty) => "thread still running".to_owned(),
                        Err(mpsc::TryRecvError::Disconnected) => {
                            "thread panicked or disconnected".to_owned()
                        }
                    };
                    panic!(
                        "timed out waiting for remote debug serial capability; {debugger_status}; reads={}, writes={}, flushes={}, read-bytes={}, write-bytes={}",
                        serial_trace.reads(),
                        serial_trace.writes(),
                        serial_trace.flushes(),
                        format_serial_chunks(&serial_trace.read_bytes()),
                        format_serial_chunks(&serial_trace.write_bytes()),
                    )
                })
                .unwrap_or_else(|error| {
                    panic!("failed to fetch remote debug serial capability: {error:#}")
                })
                .unwrap_or_else(|| panic!("remote debugger did not expose a debug serial capability"));
                let rights = timeout(Duration::from_secs(30), debug_port.rights(&mut client))
                    .await
                    .unwrap_or_else(|_| panic!("timed out waiting for remote debug serial rights"))
                    .unwrap_or_else(|error| {
                        let debugger_status = match result_rx.try_recv() {
                            Ok(result) => format!("thread exited with {result:?}"),
                            Err(mpsc::TryRecvError::Empty) => "thread still running".to_owned(),
                            Err(mpsc::TryRecvError::Disconnected) => {
                                "thread panicked or disconnected".to_owned()
                            }
                        };
                        panic!(
                            "failed to fetch remote debug serial rights: {error:#}; {debugger_status}"
                        )
                    });
                assert!(
                    rights
                        == helios_shell_protocol::system::serial::SerialRights::READ
                            | helios_shell_protocol::system::serial::SerialRights::WRITE
                            | helios_shell_protocol::system::serial::SerialRights::FLUSH,
                    "remote debug serial rights were incomplete"
                );
                timeout(Duration::from_secs(30), debug_port.drop_remote(&mut client))
                    .await
                    .unwrap_or_else(|_| panic!("timed out dropping remote debug serial capability"))
                    .unwrap_or_else(|error| panic!("failed to drop remote debug serial capability: {error}"));

                let raw_mutex = timeout(
                    Duration::from_secs(30),
                    helios_shell_protocol::system::sync::raw_mutex(&mut client),
                )
                .await
                .unwrap_or_else(|_| panic!("timed out creating remote raw mutex"))
                .unwrap_or_else(|error| panic!("failed to create remote raw mutex: {error}"));
                let raw_mutex_guard = timeout(Duration::from_secs(30), raw_mutex.lock(&mut client))
                    .await
                    .unwrap_or_else(|_| panic!("timed out locking remote raw mutex"))
                    .unwrap_or_else(|error| panic!("failed to lock remote raw mutex: {error}"));
                timeout(
                    Duration::from_secs(30),
                    raw_mutex_guard.drop_remote(&mut client),
                )
                .await
                .unwrap_or_else(|_| panic!("timed out dropping remote raw mutex guard"))
                .unwrap_or_else(|error| panic!("failed to drop remote raw mutex guard: {error}"));
                timeout(Duration::from_secs(30), raw_mutex.drop_remote(&mut client))
                    .await
                    .unwrap_or_else(|_| panic!("timed out dropping remote raw mutex"))
                    .unwrap_or_else(|error| panic!("failed to drop remote raw mutex: {error}"));

                {
                    let mut subscription = timeout(
                        Duration::from_secs(30),
                        helios_shell_protocol::system::stats::subscribe(&mut client, 1),
                    )
                    .await
                    .unwrap_or_else(|_| panic!("timed out subscribing to remote stats"))
                    .unwrap_or_else(|error| panic!("failed to subscribe to remote stats: {error}"));
                    let streamed = timeout(Duration::from_secs(30), subscription.next())
                        .await
                        .unwrap_or_else(|_| panic!("timed out waiting for streamed stats sample"))
                        .unwrap_or_else(|error| panic!("failed to read streamed stats sample: {error}"))
                        .unwrap_or_else(|| panic!("remote stats stream closed before first sample"));
                    assert_eq!(
                        streamed.processors.configured,
                        config.processor_count() as u32,
                        "streamed stats processor count must match configured processor count"
                    );
                }

                let filter = helios_shell_protocol::system::tracing::Filter {
                    min_level: Some(helios_shell_protocol::system::tracing::Level::Info),
                    target_prefixes: vec!["helios_kernel".to_owned()],
                };
                let events = timeout(
                    Duration::from_secs(30),
                    helios_shell_protocol::system::tracing::recent(&mut client, &filter, 8),
                )
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for remote tracing events"))
                .unwrap_or_else(|error| panic!("failed to fetch remote tracing events: {error}"));
                assert!(
                    events.iter().any(|event| {
                        event.target == "helios_kernel"
                            && event.fields.iter().any(|field| {
                                field.key == "message"
                                    && matches!(
                                        &field.value,
                                        helios_shell_protocol::system::tracing::Value::Text(text)
                                            if text == "Kernel initialized"
                                    )
                            })
                    }),
                    "remote tracing stream did not include the expected kernel log: {events:?}"
                );

                {
                    let mut subscription = timeout(
                        Duration::from_secs(30),
                        helios_shell_protocol::system::tracing::subscribe(&mut client, &filter),
                    )
                    .await
                    .unwrap_or_else(|_| panic!("timed out subscribing to remote tracing"))
                    .unwrap_or_else(|error| {
                        panic!("failed to subscribe to remote tracing: {error}")
                    });
                    observer.record_console_text("INFO [helios_kernel] Kernel initialized\n");
                    let event = timeout(Duration::from_secs(30), subscription.next())
                        .await
                        .unwrap_or_else(|_| panic!("timed out waiting for streamed tracing event"))
                        .unwrap_or_else(|error| panic!("failed to read streamed tracing event: {error}"))
                        .unwrap_or_else(|| panic!("remote tracing stream closed before first event"));
                    assert_eq!(event.target, "helios_kernel");
                }

                drop(client);

                let component_result = result_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap_or_else(|_| {
                        panic!("timed out waiting for hosted debugger thread to stop")
                    });
                component_thread
                    .join()
                    .unwrap_or_else(|_| panic!("hosted debugger thread panicked unexpectedly"));
                component_result.unwrap_or_else(|error| {
                    panic!("hosted debugger thread returned an error after disconnect: {error}")
                });
            });
    }
}
