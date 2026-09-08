//! Wasm component model integration.
//!
//! Submodules carve up the component runtime concerns: `cache`
//! holds compiled-component reuse logic, `fs` and `fs_path` model the
//! component-visible filesystem and its path semantics, `resources`
//! lists kernel-side resource handles surfaced to wasm, `provider` holds
//! the hand-off slots for interfaces served by kernel plugins, `runtime`
//! and `runtime_backend` host the component lifecycle and per-call state,
//! and `types` declares the component resource type aliases, and
//! `retire` carries the network handles of a socket resource that died
//! without the service that minted them.

mod cache;
mod fs;
mod fs_path;
mod provider;
mod resources;
mod retire;
mod runtime;
mod runtime_backend;
mod types;

pub(crate) use cache::ComponentCache;
pub use fs::{
    ComponentFsNodeKind, ComponentFsResourceError, ComponentResourceTableError,
    map_resource_table_error,
};
pub use fs_path::{
    ComponentFsPathError, directory_prefix, parent_path, path_is_within_directory,
    resolve_absolute_path, resolve_child_path, resolve_guest_path, strip_directory_prefix,
};
pub use provider::{
    ProviderAlreadyInstalled, ProviderError, ProviderReceiver, ProviderSender, ProviderSlot,
    provider_channel,
};
pub use resources::{
    ComponentRawMutex, ComponentRawMutexGuard, ComponentRawRwLock, ComponentRawRwLockReadGuard,
    ComponentRawRwLockWriteGuard, ComponentSerialPort, ComponentTcpBackend, ComponentTcpStream,
    ComponentUdpBackend, ComponentUdpSocket,
};
pub use retire::{
    RetiredNetworkHandle, SocketRetirementQueue, SocketRetirementSender, StoreSocketRetirement,
    retire_queued_handles,
};
pub use runtime::{
    COMPONENT_ASYNC_STACK_SIZE, ComponentOutputMode, ComponentOutputRoute, ComponentOutputSink,
    ComponentOutputStreamKind, ComponentRuntimeState, ComponentStoreData, DeadlinePollable,
    InstanceKilled, LocalOutputSink, MAX_CONCURRENT_INSTANCES, MAX_CORE_INSTANCES_PER_COMPONENT,
    MAX_MEMORIES_PER_COMPONENT, MAX_POOLED_ADDRESS_SPACE, MAX_POOLED_USER_MEMORY,
    MAX_TABLES_PER_COMPONENT, store_kernel_heap_bytes, wait_until_runtime_deadline,
};
pub use runtime_backend::{
    CompiledComponent, ComponentExecContext, ComponentExecutor, ComponentExitStatus,
    ComponentRunResult, ComponentRuntimeEngine, ComponentRuntimeFactory, ComponentWorld,
};
pub use types::{
    RawMutexGuardResource, RawMutexResource, RawRwLockReadGuardResource, RawRwLockResource,
    RawRwLockWriteGuardResource, SerialPortResource, TcpStreamResource, UdpSocketResource,
};
