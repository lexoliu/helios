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

/// Bindings for the interface a compositor reaches the display through.
///
/// Generated from the `display-host` world rather than from a program
/// world for the same reason `device-host` is: the kernel implements
/// this one interface, and the program bindings above already provide
/// every `wasi:cli` import a compositor also has.
pub mod display {
    pub mod bindings {
        use wasmtime;

        wasmtime::component::bindgen!({
            path: "../wit",
            world: "display-host",
            imports: {
                // The three calls that hand back a reader have to see
                // the store, so they can build it against one; they
                // return the reader immediately rather than awaiting.
                "helios:system/display.[method]display.changed": store | trappable,
                "helios:system/display.[method]surface.present": store | trappable,
                "helios:system/display.[method]surface.vsync": store | trappable,
                // Everything else that reaches the display engine is an
                // `async func` in the WIT and is generated against the
                // store on its own account. What is left — claiming,
                // reading back where a frame buffer landed, dropping a
                // handle — is store bookkeeping and answers without
                // waiting.
                default: trappable,
            },
            with: {
                "helios:system/display.display":
                    crate::wasmtime_adapter::component_host::DisplayHandle,
                "helios:system/display.surface":
                    crate::wasmtime_adapter::component_host::SurfaceHandle,
            },
            require_store_data_send: true,
        });
    }
}

/// Bindings for the interface a program reaches the machine's input
/// devices through.
///
/// Generated from the `input-host` world, for the reason `display-host`
/// is: the kernel implements this one interface, and the program
/// bindings above already provide every `wasi:cli` import a compositor
/// also has.
pub mod input {
    pub mod bindings {
        use wasmtime;

        wasmtime::component::bindgen!({
            path: "../wit",
            world: "input-host",
            imports: {
                // The call that hands back a reader has to see the store,
                // so it can build one against it; it returns the reader
                // immediately rather than awaiting.
                "helios:system/input.[method]device.events": store | trappable,
                // `set-led` is the one call that reaches the device and
                // is an `async func` in the WIT, generated against the
                // store on its own account. What is left — claiming,
                // listing, reading capabilities back, dropping a handle
                // — is store bookkeeping and answers without waiting.
                default: trappable,
            },
            with: {
                "helios:system/input.device":
                    crate::wasmtime_adapter::component_host::InputDeviceHandle,
            },
            require_store_data_send: true,
        });
    }
}
