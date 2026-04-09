extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use helios_kernel::wasmtime::{self, Result};
use helios_kernel::wasmtime::component::{HasSelf, Linker, Resource};
use helios_kernel::wasmtime_wasi_io::{self};
use helios_kernel::wasmtime_wasi_io::bytes::Bytes;
use helios_kernel::wasmtime_wasi_io::poll::{DynPollable, Pollable, subscribe};
use helios_kernel::wasmtime_wasi_io::streams::{
    DynInputStream, DynOutputStream, Error as IoError, InputStream, StreamError,
};

use crate::debugger_program::{DebugSerialOutputStream, RuntimeDeadlinePollable, StoreData};
use crate::debugger_program::OutputStreamKind;
use crate::debugger_wasi;
use crate::debugger_wasi::bindings::filesystem::types as p3fs;
use crate::debugger_wasi::{FsDescriptor, FsNodeKind};

pub(crate) mod cli_bindings {
    mod generated {
        use helios_kernel::wasmtime;
        use helios_kernel::wasmtime_wasi_io;

        helios_kernel::wasmtime::component::bindgen!({
            inline: "
                package helios:debugger-p2-cli;

                world imports {
                    import wasi:cli/environment@0.2.6;
                    import wasi:cli/exit@0.2.6;
                    import wasi:cli/stdin@0.2.6;
                    import wasi:cli/stdout@0.2.6;
                    import wasi:cli/stderr@0.2.6;
                    import wasi:cli/terminal-input@0.2.6;
                    import wasi:cli/terminal-output@0.2.6;
                    import wasi:cli/terminal-stdin@0.2.6;
                    import wasi:cli/terminal-stdout@0.2.6;
                    import wasi:cli/terminal-stderr@0.2.6;
                }
            ",
            path: "../../wasmtime/crates/wasi/src/p2/wit",
            imports: { default: tracing | trappable },
            with: {
                "wasi:io/poll.pollable": wasmtime_wasi_io::poll::DynPollable,
                "wasi:io/streams.input-stream": wasmtime_wasi_io::streams::DynInputStream,
                "wasi:io/streams.output-stream": wasmtime_wasi_io::streams::DynOutputStream,
                "wasi:cli/terminal-input.terminal-input": crate::debugger_wasi::p2::TerminalInput,
                "wasi:cli/terminal-output.terminal-output": crate::debugger_wasi::p2::TerminalOutput,
            },
            require_store_data_send: true,
        });
    }

    pub use self::generated::wasi::*;
}

pub(crate) mod clocks_bindings {
    mod generated {
        use helios_kernel::wasmtime;
        use helios_kernel::wasmtime_wasi_io;

        helios_kernel::wasmtime::component::bindgen!({
            inline: "
                package helios:debugger-p2-clocks;

                world imports {
                    import wasi:clocks/monotonic-clock@0.2.6;
                    import wasi:clocks/wall-clock@0.2.6;
                }
            ",
            path: "../../wasmtime/crates/wasi/src/p2/wit",
            imports: { default: tracing | trappable },
            with: {
                "wasi:io/poll.pollable": wasmtime_wasi_io::poll::DynPollable,
            },
            require_store_data_send: true,
        });
    }

    pub use self::generated::wasi::*;
}

pub(crate) mod filesystem_bindings {
    mod generated {
        use helios_kernel::wasmtime;
        use helios_kernel::wasmtime_wasi_io;

        helios_kernel::wasmtime::component::bindgen!({
            inline: "
                package helios:debugger-p2-filesystem;

                world imports {
                    import wasi:filesystem/preopens@0.2.6;
                    import wasi:filesystem/types@0.2.6;
                }
            ",
            path: "../../wasmtime/crates/wasi/src/p2/wit",
            imports: { default: tracing | trappable },
            with: {
                "wasi:io/error": wasmtime_wasi_io::bindings::wasi::io::error,
                "wasi:io/poll.pollable": wasmtime_wasi_io::poll::DynPollable,
                "wasi:io/streams.input-stream": wasmtime_wasi_io::streams::DynInputStream,
                "wasi:io/streams.output-stream": wasmtime_wasi_io::streams::DynOutputStream,
                "wasi:filesystem/types.descriptor": crate::debugger_wasi::FsDescriptor,
                "wasi:filesystem/types.directory-entry-stream": crate::debugger_wasi::p2::DirectoryEntryStream,
            },
            require_store_data_send: true,
        });
    }

    pub use self::generated::wasi::*;
}

pub(crate) mod random_bindings {
    mod generated {
        use helios_kernel::wasmtime;

        helios_kernel::wasmtime::component::bindgen!({
            inline: "
                package helios:debugger-p2-random;

                world imports {
                    import wasi:random/random@0.2.6;
                    import wasi:random/insecure@0.2.6;
                    import wasi:random/insecure-seed@0.2.6;
                }
            ",
            path: "../../wasmtime/crates/wasi/src/p2/wit",
            imports: { default: tracing | trappable },
            require_store_data_send: true,
        });
    }

    pub use self::generated::wasi::*;
}

use filesystem_bindings::filesystem::types as p2fs;

pub struct TerminalInput;
pub struct TerminalOutput;
pub struct DirectoryEntryStream {
    entries: Vec<p2fs::DirectoryEntry>,
    cursor: usize,
}

struct EmptyInputStream;

struct FileInputStream {
    bytes: Vec<u8>,
    cursor: usize,
}

impl FileInputStream {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, cursor: 0 }
    }
}

pub(crate) fn add_to_linker(linker: &mut Linker<StoreData>) -> Result<()> {
    type Data = HasSelf<StoreData>;

    cli_bindings::cli::environment::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::exit::add_to_linker::<_, Data>(linker, &Default::default(), |state| state)?;
    cli_bindings::cli::stdin::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::stdout::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::stderr::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::terminal_input::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::terminal_output::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::terminal_stdin::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::terminal_stdout::add_to_linker::<_, Data>(linker, |state| state)?;
    cli_bindings::cli::terminal_stderr::add_to_linker::<_, Data>(linker, |state| state)?;

    clocks_bindings::clocks::monotonic_clock::add_to_linker::<_, Data>(linker, |state| state)?;
    clocks_bindings::clocks::wall_clock::add_to_linker::<_, Data>(linker, |state| state)?;

    filesystem_bindings::filesystem::preopens::add_to_linker::<_, Data>(linker, |state| state)?;
    filesystem_bindings::filesystem::types::add_to_linker::<_, Data>(linker, |state| state)?;

    random_bindings::random::random::add_to_linker::<_, Data>(linker, |state| state)?;
    random_bindings::random::insecure::add_to_linker::<_, Data>(linker, |state| state)?;
    random_bindings::random::insecure_seed::add_to_linker::<_, Data>(linker, |state| state)?;
    Ok(())
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for EmptyInputStream {
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl InputStream for EmptyInputStream {
    fn read(&mut self, _: usize) -> core::result::Result<Bytes, StreamError> {
        Ok(Bytes::new())
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for FileInputStream {
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl InputStream for FileInputStream {
    fn read(&mut self, size: usize) -> core::result::Result<Bytes, StreamError> {
        if self.cursor >= self.bytes.len() {
            return Ok(Bytes::new());
        }

        let end = self.cursor.saturating_add(size).min(self.bytes.len());
        let chunk = Bytes::copy_from_slice(&self.bytes[self.cursor..end]);
        self.cursor = end;
        Ok(chunk)
    }
}

impl cli_bindings::cli::environment::Host for StoreData {
    fn get_environment(&mut self) -> Result<Vec<(String, String)>> {
        Ok(self.environment.clone())
    }

    fn get_arguments(&mut self) -> Result<Vec<String>> {
        Ok(self.arguments.clone())
    }

    fn initial_cwd(&mut self) -> Result<Option<String>> {
        Ok(Some(String::from("/")))
    }
}

impl cli_bindings::cli::exit::Host for StoreData {
    fn exit(&mut self, status: core::result::Result<(), ()>) -> Result<()> {
        let message = match status {
            Ok(()) => "guest requested wasi p2 exit success",
            Err(()) => "guest requested wasi p2 exit failure",
        };
        Err(wasmtime::Error::msg(message))
    }

    fn exit_with_code(&mut self, status_code: u8) -> Result<()> {
        Err(wasmtime::Error::msg(alloc::format!(
            "guest requested wasi p2 exit code {status_code}"
        )))
    }
}

impl cli_bindings::cli::stdin::Host for StoreData {
    fn get_stdin(&mut self) -> Result<Resource<DynInputStream>> {
        Ok(self
            .table
            .push(Box::new(EmptyInputStream) as DynInputStream)?)
    }
}

impl cli_bindings::cli::stdout::Host for StoreData {
    fn get_stdout(&mut self) -> Result<Resource<DynOutputStream>> {
        Ok(self
            .table
            .push(Box::new(DebugSerialOutputStream::from_store(
                self,
                OutputStreamKind::Stdout,
            )) as DynOutputStream)?)
    }
}

impl cli_bindings::cli::stderr::Host for StoreData {
    fn get_stderr(&mut self) -> Result<Resource<DynOutputStream>> {
        Ok(self
            .table
            .push(Box::new(DebugSerialOutputStream::from_store(
                self,
                OutputStreamKind::Stderr,
            )) as DynOutputStream)?)
    }
}

impl cli_bindings::cli::terminal_input::Host for StoreData {}
impl cli_bindings::cli::terminal_output::Host for StoreData {}

impl cli_bindings::cli::terminal_input::HostTerminalInput for StoreData {
    fn drop(&mut self, resource: Resource<TerminalInput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl cli_bindings::cli::terminal_output::HostTerminalOutput for StoreData {
    fn drop(&mut self, resource: Resource<TerminalOutput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl cli_bindings::cli::terminal_stdin::Host for StoreData {
    fn get_terminal_stdin(&mut self) -> Result<Option<Resource<TerminalInput>>> {
        Ok(None)
    }
}

impl cli_bindings::cli::terminal_stdout::Host for StoreData {
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl cli_bindings::cli::terminal_stderr::Host for StoreData {
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl clocks_bindings::clocks::monotonic_clock::Host for StoreData {
    fn now(&mut self) -> Result<clocks_bindings::clocks::monotonic_clock::Instant> {
        Ok(self.now_nanos())
    }

    fn resolution(&mut self) -> Result<clocks_bindings::clocks::monotonic_clock::Duration> {
        Ok(1)
    }

    fn subscribe_instant(&mut self, when: u64) -> Result<Resource<DynPollable>> {
        let resource = self.table.push(RuntimeDeadlinePollable::new(
            self.cpu.clone(),
            self.runtime_state.clone(),
            when
        ))?;
        subscribe(&mut self.table, resource)
    }

    fn subscribe_duration(&mut self, when: u64) -> Result<Resource<DynPollable>> {
        self.subscribe_instant(self.now_nanos().saturating_add(when))
    }
}

impl clocks_bindings::clocks::wall_clock::Host for StoreData {
    fn now(&mut self) -> Result<clocks_bindings::clocks::wall_clock::Datetime> {
        Ok(system_time_from_nanos(self.now_nanos()))
    }

    fn resolution(&mut self) -> Result<clocks_bindings::clocks::wall_clock::Datetime> {
        Ok(clocks_bindings::clocks::wall_clock::Datetime {
            seconds: 0,
            nanoseconds: 1,
        })
    }
}

impl filesystem_bindings::filesystem::preopens::Host for StoreData {
    fn get_directories(&mut self) -> Result<Vec<(Resource<FsDescriptor>, String)>> {
        let descriptor = self.filesystem.root_descriptor();
        let resource = self.table.push(descriptor)?;
        Ok(vec![(resource, String::from("/"))])
    }
}

impl filesystem_bindings::filesystem::types::Host for StoreData {
    fn filesystem_error_code(&mut self, _: Resource<IoError>) -> Result<Option<p2fs::ErrorCode>> {
        Ok(None)
    }
}

impl filesystem_bindings::filesystem::types::HostDescriptor for StoreData {
    fn read_via_stream(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        offset: u64,
    ) -> Result<core::result::Result<Resource<DynInputStream>, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        match self.filesystem.read_file(&descriptor, offset) {
            Ok(bytes) => {
                let resource = self
                    .table
                    .push(Box::new(FileInputStream::new(bytes)) as DynInputStream)?;
                Ok(Ok(resource))
            }
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn write_via_stream(
        &mut self,
        _: Resource<FsDescriptor>,
        _: u64,
    ) -> Result<core::result::Result<Resource<DynOutputStream>, p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn append_via_stream(
        &mut self,
        _: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<Resource<DynOutputStream>, p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn advise(
        &mut self,
        _: Resource<FsDescriptor>,
        _: u64,
        _: u64,
        _: p2fs::Advice,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn sync_data(
        &mut self,
        _: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Ok(()))
    }

    fn get_flags(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::DescriptorFlags, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        Ok(Ok(descriptor_flags_from_p3(descriptor.flags)))
    }

    fn get_type(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::DescriptorType, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        Ok(Ok(descriptor_type_from_kind(descriptor.kind)))
    }

    fn set_size(
        &mut self,
        _: Resource<FsDescriptor>,
        _: u64,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn set_times(
        &mut self,
        _: Resource<FsDescriptor>,
        _: p2fs::NewTimestamp,
        _: p2fs::NewTimestamp,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn read(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        length: u64,
        offset: u64,
    ) -> Result<core::result::Result<(Vec<u8>, bool), p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let length: usize = match length.try_into() {
            Ok(length) => length,
            Err(_) => return Ok(Err(p2fs::ErrorCode::Overflow)),
        };
        match self.filesystem.read_file(&descriptor, offset) {
            Ok(bytes) => {
                let eof = bytes.len() <= length;
                let bytes = if eof { bytes } else { bytes[..length].to_vec() };
                Ok(Ok((bytes, eof)))
            }
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn write(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        buffer: Vec<u8>,
        offset: u64,
    ) -> Result<core::result::Result<u64, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let offset: usize = match offset.try_into() {
            Ok(offset) => offset,
            Err(_) => return Ok(Err(p2fs::ErrorCode::Overflow)),
        };
        match self
            .filesystem
            .write_at(&descriptor, offset, &buffer, self.now_nanos())
        {
            Ok(()) => Ok(Ok(buffer.len() as u64)),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn read_directory(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<Resource<DirectoryEntryStream>, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        match self.filesystem.read_directory(&descriptor.path) {
            Ok(entries) => {
                let resource = self.table.push(DirectoryEntryStream {
                    entries: entries.into_iter().map(directory_entry_from_p3).collect(),
                    cursor: 0,
                })?;
                Ok(Ok(resource))
            }
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn sync(
        &mut self,
        _: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Ok(()))
    }

    fn create_directory_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        match self
            .filesystem
            .create_directory_at(&descriptor, &path, self.now_nanos())
        {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn stat(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::DescriptorStat, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        match self.filesystem.stat(&descriptor.path) {
            Ok(stat) => Ok(Ok(descriptor_stat_from_p3(stat))),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn stat_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path_flags: p2fs::PathFlags,
        path: String,
    ) -> Result<core::result::Result<p2fs::DescriptorStat, p2fs::ErrorCode>> {
        if path_flags.contains(p2fs::PathFlags::SYMLINK_FOLLOW) {
            return Ok(Err(p2fs::ErrorCode::Unsupported));
        }
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let path = match helios_kernel::resolve_child_path(&descriptor.path, &path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        match self.filesystem.stat(&path) {
            Ok(stat) => Ok(Ok(descriptor_stat_from_p3(stat))),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn set_times_at(
        &mut self,
        _: Resource<FsDescriptor>,
        _: p2fs::PathFlags,
        _: String,
        _: p2fs::NewTimestamp,
        _: p2fs::NewTimestamp,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn link_at(
        &mut self,
        _: Resource<FsDescriptor>,
        _: p2fs::PathFlags,
        _: String,
        _: Resource<FsDescriptor>,
        _: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn open_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path_flags: p2fs::PathFlags,
        path: String,
        open_flags: p2fs::OpenFlags,
        flags: p2fs::DescriptorFlags,
    ) -> Result<core::result::Result<Resource<FsDescriptor>, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        match self.filesystem.open_at(
            &descriptor,
            path_flags_to_p3(path_flags),
            &path,
            open_flags_to_p3(open_flags),
            descriptor_flags_to_p3(flags),
            self.now_nanos(),
        ) {
            Ok(opened) => Ok(Ok(self.table.push(opened)?)),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn readlink_at(
        &mut self,
        _: Resource<FsDescriptor>,
        _: String,
    ) -> Result<core::result::Result<String, p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn remove_directory_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        match self.filesystem.remove_directory_at(&descriptor, &path) {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn rename_at(
        &mut self,
        source_descriptor: Resource<FsDescriptor>,
        source_path: String,
        destination_descriptor: Resource<FsDescriptor>,
        destination_path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let source_descriptor = match get_fs_descriptor(self, &source_descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let destination_descriptor = match get_fs_descriptor(self, &destination_descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        match self.filesystem.rename_at(
            &source_descriptor,
            &source_path,
            &destination_descriptor,
            &destination_path,
            self.now_nanos(),
        ) {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn symlink_at(
        &mut self,
        _: Resource<FsDescriptor>,
        _: String,
        _: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        Ok(Err(p2fs::ErrorCode::Unsupported))
    }

    fn unlink_file_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<core::result::Result<(), p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        match self.filesystem.unlink_file_at(&descriptor, &path) {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn is_same_object(
        &mut self,
        a: Resource<FsDescriptor>,
        b: Resource<FsDescriptor>,
    ) -> Result<bool> {
        let left = self.table.get(&a).map_err(wasmtime::Error::from)?.clone();
        let right = self.table.get(&b).map_err(wasmtime::Error::from)?.clone();
        Ok(left.path == right.path && left.kind == right.kind)
    }

    fn metadata_hash(
        &mut self,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<core::result::Result<p2fs::MetadataHashValue, p2fs::ErrorCode>> {
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        match self.filesystem.metadata_hash(&descriptor.path) {
            Ok(hash) => Ok(Ok(metadata_hash_from_p3(hash))),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn metadata_hash_at(
        &mut self,
        descriptor: Resource<FsDescriptor>,
        path_flags: p2fs::PathFlags,
        path: String,
    ) -> Result<core::result::Result<p2fs::MetadataHashValue, p2fs::ErrorCode>> {
        if path_flags.contains(p2fs::PathFlags::SYMLINK_FOLLOW) {
            return Ok(Err(p2fs::ErrorCode::Unsupported));
        }
        let descriptor = match get_fs_descriptor(self, &descriptor) {
            Ok(descriptor) => descriptor,
            Err(error) => return Ok(Err(error)),
        };
        let path = match helios_kernel::resolve_child_path(&descriptor.path, &path) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error_code_from_path(error))),
        };
        match self.filesystem.metadata_hash(&path) {
            Ok(hash) => Ok(Ok(metadata_hash_from_p3(hash))),
            Err(error) => Ok(Err(error_code_from_p3(error))),
        }
    }

    fn drop(&mut self, resource: Resource<FsDescriptor>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl filesystem_bindings::filesystem::types::HostDirectoryEntryStream for StoreData {
    fn read_directory_entry(
        &mut self,
        resource: Resource<DirectoryEntryStream>,
    ) -> Result<core::result::Result<Option<p2fs::DirectoryEntry>, p2fs::ErrorCode>> {
        let stream = self
            .table
            .get_mut(&resource)
            .map_err(wasmtime::Error::from)?;
        let entry = stream.entries.get(stream.cursor).cloned();
        if entry.is_some() {
            stream.cursor += 1;
        }
        Ok(Ok(entry))
    }

    fn drop(&mut self, resource: Resource<DirectoryEntryStream>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl random_bindings::random::random::Host for StoreData {
    fn get_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(vec![0; len as usize])
    }

    fn get_random_u64(&mut self) -> Result<u64> {
        Ok(0)
    }
}

impl random_bindings::random::insecure::Host for StoreData {
    fn get_insecure_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(vec![0; len as usize])
    }

    fn get_insecure_random_u64(&mut self) -> Result<u64> {
        Ok(0)
    }
}

impl random_bindings::random::insecure_seed::Host for StoreData {
    fn insecure_seed(&mut self) -> Result<(u64, u64)> {
        Ok((0, 0))
    }
}

fn system_time_from_nanos(nanos: u64) -> clocks_bindings::clocks::wall_clock::Datetime {
    clocks_bindings::clocks::wall_clock::Datetime {
        seconds: nanos / 1_000_000_000,
        nanoseconds: (nanos % 1_000_000_000) as u32,
    }
}

fn get_fs_descriptor(
    store: &mut StoreData,
    resource: &Resource<FsDescriptor>,
) -> core::result::Result<FsDescriptor, p2fs::ErrorCode> {
    store
        .table
        .get(resource)
        .cloned()
        .map_err(debugger_wasi::fs_resource_error)
        .map_err(error_code_from_p3)
}

fn descriptor_type_from_kind(kind: FsNodeKind) -> p2fs::DescriptorType {
    match kind {
        FsNodeKind::Directory => p2fs::DescriptorType::Directory,
        FsNodeKind::File => p2fs::DescriptorType::RegularFile,
    }
}

fn descriptor_type_from_p3(kind: p3fs::DescriptorType) -> p2fs::DescriptorType {
    match kind {
        p3fs::DescriptorType::BlockDevice => p2fs::DescriptorType::BlockDevice,
        p3fs::DescriptorType::CharacterDevice => p2fs::DescriptorType::CharacterDevice,
        p3fs::DescriptorType::Directory => p2fs::DescriptorType::Directory,
        p3fs::DescriptorType::Fifo => p2fs::DescriptorType::Fifo,
        p3fs::DescriptorType::SymbolicLink => p2fs::DescriptorType::SymbolicLink,
        p3fs::DescriptorType::RegularFile => p2fs::DescriptorType::RegularFile,
        p3fs::DescriptorType::Socket => p2fs::DescriptorType::Socket,
        p3fs::DescriptorType::Other(_) => p2fs::DescriptorType::Unknown,
    }
}

fn descriptor_flags_from_p3(flags: p3fs::DescriptorFlags) -> p2fs::DescriptorFlags {
    let mut result = p2fs::DescriptorFlags::empty();
    if flags.contains(p3fs::DescriptorFlags::READ) {
        result |= p2fs::DescriptorFlags::READ;
    }
    if flags.contains(p3fs::DescriptorFlags::WRITE) {
        result |= p2fs::DescriptorFlags::WRITE;
    }
    if flags.contains(p3fs::DescriptorFlags::FILE_INTEGRITY_SYNC) {
        result |= p2fs::DescriptorFlags::FILE_INTEGRITY_SYNC;
    }
    if flags.contains(p3fs::DescriptorFlags::DATA_INTEGRITY_SYNC) {
        result |= p2fs::DescriptorFlags::DATA_INTEGRITY_SYNC;
    }
    if flags.contains(p3fs::DescriptorFlags::REQUESTED_WRITE_SYNC) {
        result |= p2fs::DescriptorFlags::REQUESTED_WRITE_SYNC;
    }
    if flags.contains(p3fs::DescriptorFlags::MUTATE_DIRECTORY) {
        result |= p2fs::DescriptorFlags::MUTATE_DIRECTORY;
    }
    result
}

fn descriptor_flags_to_p3(flags: p2fs::DescriptorFlags) -> p3fs::DescriptorFlags {
    let mut result = p3fs::DescriptorFlags::empty();
    if flags.contains(p2fs::DescriptorFlags::READ) {
        result |= p3fs::DescriptorFlags::READ;
    }
    if flags.contains(p2fs::DescriptorFlags::WRITE) {
        result |= p3fs::DescriptorFlags::WRITE;
    }
    if flags.contains(p2fs::DescriptorFlags::FILE_INTEGRITY_SYNC) {
        result |= p3fs::DescriptorFlags::FILE_INTEGRITY_SYNC;
    }
    if flags.contains(p2fs::DescriptorFlags::DATA_INTEGRITY_SYNC) {
        result |= p3fs::DescriptorFlags::DATA_INTEGRITY_SYNC;
    }
    if flags.contains(p2fs::DescriptorFlags::REQUESTED_WRITE_SYNC) {
        result |= p3fs::DescriptorFlags::REQUESTED_WRITE_SYNC;
    }
    if flags.contains(p2fs::DescriptorFlags::MUTATE_DIRECTORY) {
        result |= p3fs::DescriptorFlags::MUTATE_DIRECTORY;
    }
    result
}

fn open_flags_to_p3(flags: p2fs::OpenFlags) -> p3fs::OpenFlags {
    let mut result = p3fs::OpenFlags::empty();
    if flags.contains(p2fs::OpenFlags::CREATE) {
        result |= p3fs::OpenFlags::CREATE;
    }
    if flags.contains(p2fs::OpenFlags::DIRECTORY) {
        result |= p3fs::OpenFlags::DIRECTORY;
    }
    if flags.contains(p2fs::OpenFlags::EXCLUSIVE) {
        result |= p3fs::OpenFlags::EXCLUSIVE;
    }
    if flags.contains(p2fs::OpenFlags::TRUNCATE) {
        result |= p3fs::OpenFlags::TRUNCATE;
    }
    result
}

fn path_flags_to_p3(flags: p2fs::PathFlags) -> p3fs::PathFlags {
    let mut result = p3fs::PathFlags::empty();
    if flags.contains(p2fs::PathFlags::SYMLINK_FOLLOW) {
        result |= p3fs::PathFlags::SYMLINK_FOLLOW;
    }
    result
}

fn descriptor_stat_from_p3(stat: p3fs::DescriptorStat) -> p2fs::DescriptorStat {
    p2fs::DescriptorStat {
        type_: descriptor_type_from_p3(stat.type_),
        link_count: stat.link_count,
        size: stat.size,
        data_access_timestamp: stat.data_access_timestamp.map(datetime_from_p3),
        data_modification_timestamp: stat.data_modification_timestamp.map(datetime_from_p3),
        status_change_timestamp: stat.status_change_timestamp.map(datetime_from_p3),
    }
}

fn metadata_hash_from_p3(hash: p3fs::MetadataHashValue) -> p2fs::MetadataHashValue {
    p2fs::MetadataHashValue {
        lower: hash.lower,
        upper: hash.upper,
    }
}

fn directory_entry_from_p3(entry: p3fs::DirectoryEntry) -> p2fs::DirectoryEntry {
    p2fs::DirectoryEntry {
        type_: descriptor_type_from_p3(entry.type_),
        name: entry.name,
    }
}

fn datetime_from_p3(
    instant: debugger_wasi::bindings::clocks::system_clock::Instant,
) -> p2fs::Datetime {
    p2fs::Datetime {
        seconds: instant
            .seconds
            .try_into()
            .expect("preview2 filesystem cannot represent a negative timestamp"),
        nanoseconds: instant.nanoseconds,
    }
}

fn error_code_from_path(error: helios_kernel::ComponentFsPathError) -> p2fs::ErrorCode {
    match error {
        helios_kernel::ComponentFsPathError::InvalidBasePath => p2fs::ErrorCode::Invalid,
        helios_kernel::ComponentFsPathError::NotPermitted => p2fs::ErrorCode::NotPermitted,
    }
}

fn error_code_from_p3(error: p3fs::ErrorCode) -> p2fs::ErrorCode {
    match error {
        p3fs::ErrorCode::Access => p2fs::ErrorCode::Access,
        p3fs::ErrorCode::Already => p2fs::ErrorCode::Already,
        p3fs::ErrorCode::BadDescriptor => p2fs::ErrorCode::BadDescriptor,
        p3fs::ErrorCode::Busy => p2fs::ErrorCode::Busy,
        p3fs::ErrorCode::Deadlock => p2fs::ErrorCode::Deadlock,
        p3fs::ErrorCode::Quota => p2fs::ErrorCode::Quota,
        p3fs::ErrorCode::Exist => p2fs::ErrorCode::Exist,
        p3fs::ErrorCode::FileTooLarge => p2fs::ErrorCode::FileTooLarge,
        p3fs::ErrorCode::IllegalByteSequence => p2fs::ErrorCode::IllegalByteSequence,
        p3fs::ErrorCode::InProgress => p2fs::ErrorCode::InProgress,
        p3fs::ErrorCode::Interrupted => p2fs::ErrorCode::Interrupted,
        p3fs::ErrorCode::Invalid => p2fs::ErrorCode::Invalid,
        p3fs::ErrorCode::Io => p2fs::ErrorCode::Io,
        p3fs::ErrorCode::IsDirectory => p2fs::ErrorCode::IsDirectory,
        p3fs::ErrorCode::Loop => p2fs::ErrorCode::Loop,
        p3fs::ErrorCode::TooManyLinks => p2fs::ErrorCode::TooManyLinks,
        p3fs::ErrorCode::MessageSize => p2fs::ErrorCode::MessageSize,
        p3fs::ErrorCode::NameTooLong => p2fs::ErrorCode::NameTooLong,
        p3fs::ErrorCode::NoDevice => p2fs::ErrorCode::NoDevice,
        p3fs::ErrorCode::NoEntry => p2fs::ErrorCode::NoEntry,
        p3fs::ErrorCode::NoLock => p2fs::ErrorCode::NoLock,
        p3fs::ErrorCode::InsufficientMemory => p2fs::ErrorCode::InsufficientMemory,
        p3fs::ErrorCode::InsufficientSpace => p2fs::ErrorCode::InsufficientSpace,
        p3fs::ErrorCode::NotDirectory => p2fs::ErrorCode::NotDirectory,
        p3fs::ErrorCode::NotEmpty => p2fs::ErrorCode::NotEmpty,
        p3fs::ErrorCode::NotRecoverable => p2fs::ErrorCode::NotRecoverable,
        p3fs::ErrorCode::Unsupported => p2fs::ErrorCode::Unsupported,
        p3fs::ErrorCode::NoTty => p2fs::ErrorCode::NoTty,
        p3fs::ErrorCode::NoSuchDevice => p2fs::ErrorCode::NoSuchDevice,
        p3fs::ErrorCode::Overflow => p2fs::ErrorCode::Overflow,
        p3fs::ErrorCode::NotPermitted => p2fs::ErrorCode::NotPermitted,
        p3fs::ErrorCode::Pipe => p2fs::ErrorCode::Pipe,
        p3fs::ErrorCode::ReadOnly => p2fs::ErrorCode::ReadOnly,
        p3fs::ErrorCode::InvalidSeek => p2fs::ErrorCode::InvalidSeek,
        p3fs::ErrorCode::TextFileBusy => p2fs::ErrorCode::TextFileBusy,
        p3fs::ErrorCode::CrossDevice => p2fs::ErrorCode::CrossDevice,
        p3fs::ErrorCode::Other(_) => {
            panic!("preview2 filesystem cannot represent preview3 error-code::other")
        }
    }
}
