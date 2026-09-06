pub mod debugger {
    pub mod bindings {
        use wasmtime;

        wasmtime::component::bindgen!({
            path: "../wit",
            world: "debugger",
            imports: {
                "helios:system/programs.[method]child.stdin": store | trappable,
                "helios:system/programs.[method]child.stdout": store | trappable,
                "helios:system/programs.[method]child.stderr": store | trappable,
                default: async | store | trappable
            },
            exports: { default: async },
            with: {
                "helios:system/programs.child": crate::ChildHandle,
                "helios:system/serial.serial-port": crate::ComponentSerialPort,
                "helios:system/vsock.vsock-stream": crate::wasmtime_adapter::component_host::ComponentVsockStream,
                "helios:system/vsock.vsock-listener": crate::wasmtime_adapter::component_host::ComponentVsockListener,
                "helios:system/sync.raw-mutex": crate::ComponentRawMutex,
                "helios:system/sync.raw-mutex-guard": crate::ComponentRawMutexGuard,
                "helios:system/sync.raw-rw-lock": crate::ComponentRawRwLock,
                "helios:system/sync.raw-rw-lock-read-guard": crate::ComponentRawRwLockReadGuard,
                "helios:system/sync.raw-rw-lock-write-guard": crate::ComponentRawRwLockWriteGuard,
            },
            require_store_data_send: true,
        });
    }
}

pub mod program {
    pub mod bindings {
        use wasmtime;

        wasmtime::component::bindgen!({
            path: "../wit",
            world: "init",
            imports: {
                "helios:system/programs.[method]child.stdin": store | trappable,
                "helios:system/programs.[method]child.stdout": store | trappable,
                "helios:system/programs.[method]child.stderr": store | trappable,
                default: async | store | trappable
            },
            exports: { default: async },
            with: {
                "helios:system/programs.child": crate::ChildHandle,
                "helios:system/serial.serial-port": crate::ComponentSerialPort,
                "helios:system/vsock.vsock-stream": crate::wasmtime_adapter::component_host::ComponentVsockStream,
                "helios:system/vsock.vsock-listener": crate::wasmtime_adapter::component_host::ComponentVsockListener,
                "helios:system/sync.raw-mutex": crate::ComponentRawMutex,
                "helios:system/sync.raw-mutex-guard": crate::ComponentRawMutexGuard,
                "helios:system/sync.raw-rw-lock": crate::ComponentRawRwLock,
                "helios:system/sync.raw-rw-lock-read-guard": crate::ComponentRawRwLockReadGuard,
                "helios:system/sync.raw-rw-lock-write-guard": crate::ComponentRawRwLockWriteGuard,
            },
            require_store_data_send: true,
        });
    }
}

/// Bindings for the interface a driver plugin reaches its device through.
///
/// Generated from the `device-host` world rather than from `device-driver`:
/// the kernel implements this one interface and the program bindings above
/// already provide every `wasi:cli` import a driver also has.
pub mod device {
    pub mod bindings {
        use wasmtime;

        wasmtime::component::bindgen!({
            path: "../wit",
            world: "device-host",
            imports: {
                // The one call that hands back a stream has to see the
                // store, so it can build the reader against it.
                "helios:system/device.[method]grant.interrupts": store | trappable,
                default: trappable,
            },
            with: {
                "helios:system/device.grant": crate::GrantHandle,
                "helios:system/device.dma-buffer": crate::DmaBufferHandle,
            },
            require_store_data_send: true,
        });
    }
}
