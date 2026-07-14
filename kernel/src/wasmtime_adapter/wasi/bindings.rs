mod generated {
    use wasmtime;

    wasmtime::component::bindgen!({
        path: "../wit",
        world: "wasi:cli/command",
        imports: {
            "wasi:cli/stdin": store | trappable,
            "wasi:cli/stdout": store | trappable,
            "wasi:cli/stderr": store | trappable,
            "wasi:filesystem/types.[method]descriptor.stat": async | trappable,
            "wasi:filesystem/types.[method]descriptor.stat-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.open-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.read-via-stream": store | trappable,
            "wasi:filesystem/types.[method]descriptor.write-via-stream": store | trappable,
            "wasi:filesystem/types.[method]descriptor.append-via-stream": store | trappable,
            "wasi:filesystem/types.[method]descriptor.read-directory": store | trappable,
            "wasi:filesystem/types.[method]descriptor.create-directory-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.unlink-file-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.rename-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.remove-directory-at": async | trappable,
            "wasi:filesystem/types.[method]descriptor.metadata-hash": async | trappable,
            "wasi:filesystem/types.[method]descriptor.metadata-hash-at": async | trappable,
            "wasi:sockets/types.[method]tcp-socket.bind": async | trappable,
            "wasi:sockets/types.[method]tcp-socket.listen": store | trappable,
            "wasi:sockets/types.[method]tcp-socket.send": store | trappable,
            "wasi:sockets/types.[method]tcp-socket.receive": store | trappable,
            "wasi:sockets/types.[method]udp-socket.bind": async | trappable,
            "wasi:sockets/types.[method]udp-socket.connect": async | trappable,
            default: trappable,
        },
        exports: { default: async },
        require_store_data_send: true,
        with: {
            "wasi:cli/terminal-input.terminal-input": crate::wasmtime_adapter::wasi::TerminalInput,
            "wasi:cli/terminal-output.terminal-output": crate::wasmtime_adapter::wasi::TerminalOutput,
            "wasi:filesystem/types.descriptor": crate::wasmtime_adapter::wasi::FsDescriptor,
            "wasi:sockets/types.tcp-socket": crate::wasmtime_adapter::wasi::TcpSocket,
            "wasi:sockets/types.udp-socket": crate::wasmtime_adapter::wasi::UdpSocket,
        },
        trappable_error_type: {
            "wasi:filesystem/types.error-code" => crate::wasmtime_adapter::wasi::FsError,
        },
    });
}

pub use self::generated::wasi::*;
