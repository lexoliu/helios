pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit",
        world: "debugger",
        imports: { default: async | trappable },
        exports: { default: async },
        with: {
            "wasi": wasmtime_wasi::p3::bindings,
            "helios:system/serial.serial-port": crate::serial_host::HostedSerialPort,
            "helios:system/sync.raw-mutex": crate::sync_host::HostedRawMutex,
            "helios:system/sync.raw-mutex-guard": crate::sync_host::HostedRawMutexGuard,
            "helios:system/sync.raw-rw-lock": crate::sync_host::HostedRawRwLock,
            "helios:system/sync.raw-rw-lock-read-guard": crate::sync_host::HostedRawRwLockReadGuard,
            "helios:system/sync.raw-rw-lock-write-guard": crate::sync_host::HostedRawRwLockWriteGuard,
        },
        require_store_data_send: true,
    });
}
