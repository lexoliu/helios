use std::fs;
use std::path::Path;
use std::thread;
use std::time::Instant as StdInstant;

use helios_kernel::{
    EmbeddedComponent, EmbeddedDebugger, EmbeddedInit, InstanceExecutionTransition,
    InstanceRegistry, Kernel, ProgramService, RegisteredInstance, embedded_debugger, embedded_init,
};
use tempfile::TempDir;
use thiserror::Error;
use wasmtime::component::{HasSelf, ResourceTable};
use wasmtime::{CallHook, Config, Engine, ResourceLimiter, Store};
use wasmtime_wasi::cli;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::config::HostedConfig;
use crate::cpu::HostedCpu;
use crate::observer_buffer::SharedObserverBuffer;
use crate::serial_host::HostedSerialIo;

const WASMTIME_TARGET: &str = env!("HELIOS_HOSTED_TARGET");

pub fn spawn_init(
    _kernel: &Kernel<HostedCpu>,
    config: &HostedConfig,
    observer: SharedObserverBuffer,
    instance_registry: InstanceRegistry,
    program_service: Option<ProgramService<HostedCpu>>,
    boot_started_at: StdInstant,
) {
    let init = embedded_init()
        .unwrap_or_else(|| panic!("no embedded init program found; set HELIOS_INIT_WASM"));
    let config = config.clone();

    thread::Builder::new()
        .name("helios-init".to_owned())
        .spawn(move || {
            run_component_thread(
                EmbeddedHostedProgram::Init(init),
                config,
                observer,
                instance_registry,
                program_service,
                boot_started_at,
            )
        })
        .unwrap_or_else(|error| panic!("failed to spawn hosted init thread: {error}"));
}

pub fn spawn_debugger(
    _kernel: &Kernel<HostedCpu>,
    config: &HostedConfig,
    observer: SharedObserverBuffer,
    instance_registry: InstanceRegistry,
    program_service: Option<ProgramService<HostedCpu>>,
    boot_started_at: StdInstant,
) {
    let debugger = embedded_debugger()
        .unwrap_or_else(|| panic!("no embedded debugger program found; set HELIOS_DEBUGGER_WASM"));
    let config = config.clone();

    thread::Builder::new()
        .name("helios-debugger".to_owned())
        .spawn(move || {
            run_component_thread(
                EmbeddedHostedProgram::Debugger(debugger),
                config,
                observer,
                instance_registry,
                program_service,
                boot_started_at,
            )
        })
        .unwrap_or_else(|error| panic!("failed to spawn hosted debugger thread: {error}"));
}

fn run_component_thread(
    program: EmbeddedHostedProgram,
    config: HostedConfig,
    observer: SharedObserverBuffer,
    instance_registry: InstanceRegistry,
    program_service: Option<ProgramService<HostedCpu>>,
    boot_started_at: StdInstant,
) {
    run_component_thread_with_serial(
        program,
        config,
        observer,
        instance_registry,
        program_service,
        boot_started_at,
        Some(HostedSerialIo::new()),
    )
    .unwrap_or_else(|error| panic!("failed to launch embedded component: {error}"));
}

fn run_component_thread_with_serial(
    program: EmbeddedHostedProgram,
    config: HostedConfig,
    observer: SharedObserverBuffer,
    instance_registry: InstanceRegistry,
    program_service: Option<ProgramService<HostedCpu>>,
    boot_started_at: StdInstant,
    serial: Option<HostedSerialIo>,
) -> Result<(), HostedProgramError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("failed to build hosted component runtime: {error}"));

    runtime.block_on(run_component_with_serial(
        program,
        &config,
        observer,
        instance_registry,
        program_service,
        boot_started_at,
        serial,
    ))
}

async fn run_component_with_serial(
    program: EmbeddedHostedProgram,
    config: &HostedConfig,
    observer: SharedObserverBuffer,
    instance_registry: InstanceRegistry,
    program_service: Option<ProgramService<HostedCpu>>,
    boot_started_at: StdInstant,
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
    add_generated_bindings_to_linker(&program, &mut linker)
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
    let instance =
        instance_registry.register(program.name().to_string(), nanos_since(boot_started_at));
    let mut store = Store::new(
        &engine,
        StoreData {
            table: ResourceTable::new(),
            wasi,
            serial,
            processor_count: config.processor_count() as u32,
            boot_started_at,
            observer,
            instance_registry,
            program_service,
            instance,
            mounted_bootfs,
        },
    );
    store.limiter(|state| state);
    store.call_hook(|mut caller, hook| {
        caller.data_mut().record_call_hook(hook);
        Ok(())
    });

    let result = match &program {
        EmbeddedHostedProgram::Init(_) => {
            let instance = crate::program_bindings::bindings::Init::instantiate_async(
                &mut store, &component, &linker,
            )
            .await
            .map_err(HostedProgramError::InstantiateComponent)?;
            store
                .run_concurrent(async move |accessor| {
                    instance.wasi_cli_run().call_run(accessor).await
                })
                .await
                .map_err(HostedProgramError::RunConcurrent)?
                .map_err(HostedProgramError::RunComponent)?
        }
        EmbeddedHostedProgram::Debugger(_) => {
            let instance = crate::debugger_bindings::bindings::Debugger::instantiate_async(
                &mut store, &component, &linker,
            )
            .await
            .map_err(HostedProgramError::InstantiateComponent)?;
            store
                .run_concurrent(async move |accessor| {
                    instance.wasi_cli_run().call_run(accessor).await
                })
                .await
                .map_err(HostedProgramError::RunConcurrent)?
                .map_err(HostedProgramError::RunComponent)?
        }
    };
    result.map_err(|()| HostedProgramError::GuestFailed(program.name().to_owned()))?;
    Ok(())
}

fn add_generated_bindings_to_linker(
    program: &EmbeddedHostedProgram,
    linker: &mut wasmtime::component::Linker<StoreData>,
) -> wasmtime::Result<()> {
    match program {
        EmbeddedHostedProgram::Init(_) => {
            crate::program_bindings::bindings::helios::system::programs::add_to_linker::<
                _,
                HasSelf<_>,
            >(linker, |state| state)?;
            crate::program_bindings::bindings::helios::system::serial::add_to_linker::<
                _,
                HasSelf<_>,
            >(linker, |state| state)?;
            crate::program_bindings::bindings::helios::system::sync::add_to_linker::<_, HasSelf<_>>(
                linker,
                |state| state,
            )?;
        }
        EmbeddedHostedProgram::Debugger(_) => {
            crate::debugger_bindings::bindings::helios::system::programs::add_to_linker::<
                _,
                HasSelf<_>,
            >(linker, |state| state)?;
            crate::debugger_bindings::bindings::helios::system::serial::add_to_linker::<
                _,
                HasSelf<_>,
            >(linker, |state| state)?;
            crate::debugger_bindings::bindings::helios::system::sync::add_to_linker::<_, HasSelf<_>>(
                linker,
                |state| state,
            )?;
        }
    }
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
    pub(crate) boot_started_at: StdInstant,
    pub(crate) observer: SharedObserverBuffer,
    pub(crate) instance_registry: InstanceRegistry,
    pub(crate) program_service: Option<ProgramService<HostedCpu>>,
    pub(crate) instance: RegisteredInstance,
    mounted_bootfs: Option<MountedBootFs>,
}

impl StoreData {
    pub(crate) fn boot_now_nanos(&self) -> u64 {
        nanos_since(self.boot_started_at)
    }

    fn record_call_hook(&mut self, hook: CallHook) {
        let transition = match hook {
            CallHook::CallingWasm | CallHook::ReturningFromHost => {
                InstanceExecutionTransition::Resume
            }
            CallHook::ReturningFromWasm | CallHook::CallingHost => {
                InstanceExecutionTransition::Pause
            }
        };
        self.instance.transition(transition, self.boot_now_nanos());
    }
}

impl ResourceLimiter for StoreData {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allow = maximum.is_none_or(|maximum| desired <= maximum);
        if allow {
            self.instance
                .set_memory_bytes(u64::try_from(desired).expect("desired memory exceeds u64"));
        }
        Ok(allow)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(maximum.is_none_or(|maximum| desired <= maximum))
    }
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

fn nanos_since(started_at: StdInstant) -> u64 {
    u64::try_from(started_at.elapsed().as_nanos())
        .expect("hosted monotonic nanoseconds do not fit into u64")
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

    use helios_kernel::{InstanceRegistry, embedded_debugger};
    use tokio::time::timeout;
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
    use wasm_encoder::{
        CodeSection, ExportKind, ExportSection, Function, FunctionSection, Instruction, Module,
        TypeSection,
    };

    use super::{EmbeddedHostedProgram, HostedConfig, run_component_thread_with_serial};
    use crate::observer_buffer::ObserverBuffer;
    use crate::program_host;
    use crate::runtime::HostedMachine;
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

    fn spin_start_module() -> Vec<u8> {
        let mut types = TypeSection::new();
        types.ty().function([], []);

        let mut functions = FunctionSection::new();
        functions.function(0);

        let mut exports = ExportSection::new();
        exports.export("_start", ExportKind::Func, 0);

        let mut code = CodeSection::new();
        let mut function = Function::new([]);
        function.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        function.instruction(&Instruction::Br(0));
        function.instruction(&Instruction::End);
        function.instruction(&Instruction::End);
        code.function(&function);

        let mut module = Module::new();
        module.section(&types);
        module.section(&functions);
        module.section(&exports);
        module.section(&code);
        module.finish()
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
                let instance_registry = InstanceRegistry::new();
                let boot_started_at = StdInstant::now();
                let (result_tx, result_rx) = mpsc::sync_channel(1);
                let component_thread = std::thread::spawn(move || {
                    let result = run_component_thread_with_serial(
                        EmbeddedHostedProgram::Debugger(debugger),
                        component_config,
                        component_observer,
                        instance_registry,
                        None,
                        boot_started_at,
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

                let instances = timeout(
                    Duration::from_secs(30),
                    helios_shell_protocol::system::instances::snapshot(&mut client),
                )
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for remote instances snapshot"))
                .unwrap_or_else(|error| panic!("failed to fetch remote instances: {error:#}"));
                assert!(
                    instances.iter().any(|instance| instance.name == "debugger"),
                    "remote instances snapshot did not include the debugger: {instances:?}"
                );

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

    #[test]
    fn embedded_debugger_launches_remote_programs() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| {
                panic!("failed to build tokio runtime for hosted debugger launch test: {error}")
            })
            .block_on(async {
                let debugger = embedded_debugger()
                    .unwrap_or_else(|| panic!("embedded debugger component is missing"));
                let config = HostedConfig::from_env();
                let observer = ObserverBuffer::new(StdInstant::now());
                let machine = HostedMachine::new(&config);
                let program_service = program_host::create_program_service(
                    &config,
                    crate::cpu::HostedCpu::new(machine.bootstrap_processor(), machine.clone()),
                    machine.instance_registry(),
                );

                let (serial, peer, _) = HostedSerialIo::traced_duplex_pair(4096);
                let component_config = config.clone();
                let component_observer = observer.clone();
                let instance_registry = machine.instance_registry();
                let boot_started_at = StdInstant::now();
                let (result_tx, result_rx) = mpsc::sync_channel(1);
                let component_thread = std::thread::spawn(move || {
                    let result = run_component_thread_with_serial(
                        EmbeddedHostedProgram::Debugger(debugger),
                        component_config,
                        component_observer,
                        instance_registry,
                        Some(program_service),
                        boot_started_at,
                        Some(serial),
                    );
                    result_tx.send(result).unwrap_or_else(|error| {
                        panic!("failed to send debugger launch-test result: {error}")
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
                            panic!("hosted debugger launch-test thread panicked unexpectedly")
                        });
                        panic!("debugger thread exited before launch test: {result:?}");
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        component_thread.join().unwrap_or_else(|_| {
                            panic!("hosted debugger launch-test thread panicked unexpectedly")
                        });
                        panic!("debugger thread disconnected before launch test");
                    }
                }

                let instance_id = timeout(
                    Duration::from_secs(30),
                    helios_shell_protocol::system::programs::launch(
                        &mut client,
                        &helios_shell_protocol::system::programs::LaunchRequest {
                            name: "spin.wasm".to_owned(),
                            wasm: spin_start_module(),
                        },
                    ),
                )
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for remote programs.launch"))
                .unwrap_or_else(|error| panic!("remote programs.launch failed: {error:#}"));

                let instances = timeout(
                    Duration::from_secs(30),
                    helios_shell_protocol::system::instances::snapshot(&mut client),
                )
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for remote instances after launch"))
                .unwrap_or_else(|error| {
                    panic!("failed to fetch remote instances after launch: {error:#}")
                });

                assert!(
                    instances.iter().any(|instance| {
                        instance.id == instance_id && instance.name == "spin.wasm"
                    }),
                    "launched program did not appear in remote instances: {instances:?}"
                );
            });
    }
}
