use std::process;
use std::thread;

use helios_kernel::{EmbeddedInit, Kernel, embedded_init};
use thiserror::Error;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p3::bindings::Command;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bootfs_mount::{MountError, MountedBootFs};
use crate::config::HostedConfig;
use crate::cpu::HostedCpu;

const GUEST_ROOT_PATH: &str = "/";

/// Launches the embedded init component, if one was compiled into the binary.
///
/// Hosted mode runs init on a dedicated OS thread with its own Tokio runtime.
/// This keeps the kernel executor free while still using Wasmtime's upstream
/// async component/WASI stack.
pub fn spawn(_kernel: &Kernel<HostedCpu>, config: &HostedConfig) {
    let init = embedded_init().unwrap_or_else(|| {
        panic!("no embedded init program found; set HELIOS_INIT_WASM when building")
    });
    let config = config.clone();

    thread::Builder::new()
        .name("helios-init".to_owned())
        .spawn(move || run_init_thread(init, config))
        .unwrap_or_else(|error| panic!("failed to spawn hosted init thread: {error}"));
}

fn run_init_thread(init: EmbeddedInit, config: HostedConfig) -> ! {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("failed to build init tokio runtime: {error}"));

    let exit_code = runtime.block_on(async move {
        run_init(init, config)
            .await
            .unwrap_or_else(|error| panic!("failed to launch embedded init component: {error}"))
    });

    tracing::info!("init exited with code={exit_code}");
    process::exit(exit_code);
}

async fn run_init(init: EmbeddedInit, config: HostedConfig) -> Result<i32, InitError> {
    let engine = build_engine(init.component().target())?;
    let component = unsafe { Component::deserialize(&engine, init.component().artifact()) }
        .map_err(InitError::DeserializeComponent)?;

    let mounted_bootfs = MountedBootFs::mount(init.bootfs(), config.init_wasi_root())
        .map_err(InitError::MountBootFs)?;
    let store_data = create_store_data(init, mounted_bootfs, &config)?;

    let mut linker = Linker::<StoreData>::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker).map_err(InitError::LinkWasi)?;

    let mut store = Store::new(&engine, store_data);
    let command = Command::instantiate_async(&mut store, &component, &linker)
        .await
        .map_err(InitError::InstantiateComponent)?;
    let result = store
        .run_concurrent(async move |store| command.wasi_cli_run().call_run(store).await)
        .await
        .map_err(InitError::RunComponent)?;
    let result = result.map_err(InitError::RunComponent)?;

    Ok(match result {
        Ok(()) => 0,
        Err(()) => 1,
    })
}

fn build_engine(target: &str) -> Result<Engine, InitError> {
    let mut config = Config::new();
    config
        .target(target)
        .map_err(|error| InitError::ConfigureTarget(target.to_owned(), error))?;
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config).map_err(InitError::CreateEngine)
}

fn create_store_data(
    init: EmbeddedInit,
    mounted_bootfs: MountedBootFs,
    config: &HostedConfig,
) -> Result<StoreData, InitError> {
    let mut builder = WasiCtxBuilder::new();
    let argv = build_argv(init, config);
    let env = config.init_env();

    builder
        .inherit_stdout()
        .inherit_stderr()
        .args(&argv)
        .envs(env);
    builder
        .preopened_dir(
            mounted_bootfs.path(),
            GUEST_ROOT_PATH,
            DirPerms::READ,
            FilePerms::READ,
        )
        .map_err(InitError::CreatePreopen)?;

    Ok(StoreData {
        wasi: builder.build(),
        table: ResourceTable::new(),
        _mounted_bootfs: mounted_bootfs,
    })
}

fn build_argv(init: EmbeddedInit, config: &HostedConfig) -> Vec<String> {
    let mut argv = Vec::with_capacity(config.init_args().len() + 1);
    argv.push(init.argv0().to_owned());
    argv.extend(config.init_args().iter().cloned());
    argv
}

struct StoreData {
    wasi: WasiCtx,
    table: ResourceTable,
    // Keep the temporary root alive for the entire lifetime of the store.
    _mounted_bootfs: MountedBootFs,
}

impl WasiView for StoreData {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

#[derive(Debug, Error)]
enum InitError {
    #[error("failed to configure Wasmtime target {0:?}: {1}")]
    ConfigureTarget(String, wasmtime::Error),
    #[error("failed to initialize Wasmtime engine: {0}")]
    CreateEngine(wasmtime::Error),
    #[error("failed to deserialize embedded init component: {0}")]
    DeserializeComponent(wasmtime::Error),
    #[error("failed to mount init bootfs: {0}")]
    MountBootFs(MountError),
    #[error("failed to expose bootfs as a WASI preopen: {0}")]
    CreatePreopen(wasmtime::Error),
    #[error("failed to add WASI Preview 3 host bindings: {0}")]
    LinkWasi(wasmtime::Error),
    #[error("failed to instantiate init component: {0}")]
    InstantiateComponent(wasmtime::Error),
    #[error("init component trapped: {0}")]
    RunComponent(wasmtime::Error),
}
