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
                "helios:system/net.tcp-stream": crate::ComponentTcpStream,
                "helios:system/net.udp-socket": crate::ComponentUdpSocket,
                "helios:system/programs.child": crate::ChildHandle,
                "helios:system/serial.serial-port": crate::ComponentSerialPort,
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
                "helios:system/net.tcp-stream": crate::ComponentTcpStream,
                "helios:system/net.udp-socket": crate::ComponentUdpSocket,
                "helios:system/programs.child": crate::ChildHandle,
                "helios:system/serial.serial-port": crate::ComponentSerialPort,
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
