pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit/deps/helios-system/sync.wit",
        world: "sync-host",
        imports: { default: trappable },
        with: {
            "helios:system/sync.raw-mutex": crate::sync_host::HostedRawMutex,
            "helios:system/sync.raw-mutex-guard": crate::sync_host::HostedRawMutexGuard,
            "helios:system/sync.raw-rw-lock": crate::sync_host::HostedRawRwLock,
            "helios:system/sync.raw-rw-lock-read-guard": crate::sync_host::HostedRawRwLockReadGuard,
            "helios:system/sync.raw-rw-lock-write-guard": crate::sync_host::HostedRawRwLockWriteGuard,
        },
    });
}
