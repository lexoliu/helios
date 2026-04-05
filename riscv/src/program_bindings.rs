pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit",
        world: "init",
        imports: { default: async | store | trappable },
        exports: { default: async },
        with: {
            "helios:system/serial.serial-port": crate::debugger_program::SbiSerialPort,
            "helios:system/sync.raw-mutex": crate::debugger_program::SbiRawMutex,
            "helios:system/sync.raw-mutex-guard": crate::debugger_program::SbiRawMutexGuard,
            "helios:system/sync.raw-rw-lock": crate::debugger_program::SbiRawRwLock,
            "helios:system/sync.raw-rw-lock-read-guard":
                crate::debugger_program::SbiRawRwLockReadGuard,
            "helios:system/sync.raw-rw-lock-write-guard":
                crate::debugger_program::SbiRawRwLockWriteGuard,
        },
        require_store_data_send: true,
    });
}
