extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures::channel::oneshot;
use helios_hal::io::IoError;
use helios_kernel::{EmbeddedBootFile, EmbeddedBootFs, embedded_init};
use helios_kernel::wasmtime::{self, Result};
use helios_kernel::wasmtime::component::{
    Access, Accessor, FutureReader, HasSelf, Linker, Resource, Source,
    StreamConsumer, StreamReader, StreamResult,
};

use crate::debugger_program::{OutputStreamKind, StoreData};

pub(crate) type FsNodeKind = helios_kernel::ComponentFsNodeKind;

pub(crate) mod p2 {
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
}

pub(crate) mod bindings {
    mod generated {
        use helios_kernel::wasmtime;

        helios_kernel::wasmtime::component::bindgen!({
            path: "../../wasmtime/crates/wasi/src/p3/wit",
            world: "wasi:cli/command",
            imports: {
                "wasi:cli/stdin": store | trappable,
                "wasi:cli/stdout": store | trappable,
                "wasi:cli/stderr": store | trappable,
                "wasi:filesystem/types.[method]descriptor.read-via-stream": store | trappable,
                "wasi:filesystem/types.[method]descriptor.write-via-stream": store | trappable,
                "wasi:filesystem/types.[method]descriptor.append-via-stream": store | trappable,
                "wasi:filesystem/types.[method]descriptor.read-directory": store | trappable,
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
                "wasi:cli/terminal-input.terminal-input": crate::debugger_wasi::TerminalInput,
                "wasi:cli/terminal-output.terminal-output": crate::debugger_wasi::TerminalOutput,
                "wasi:filesystem/types.descriptor": crate::debugger_wasi::FsDescriptor,
                "wasi:sockets/types.tcp-socket": crate::debugger_wasi::TcpSocket,
                "wasi:sockets/types.udp-socket": crate::debugger_wasi::UdpSocket,
            },
            trappable_error_type: {
                "wasi:filesystem/types.error-code" => crate::debugger_wasi::FsError,
            },
        });
    }

    pub use self::generated::wasi::*;
}

use bindings as wasi;
use wasi::cli::types as cli_types;
use wasi::filesystem::types as fs_types;
use wasi::sockets::ip_name_lookup;
use wasi::sockets::types as socket_types;

pub(crate) fn add_to_linker(linker: &mut Linker<StoreData>) -> Result<()> {
    type Data = HasSelf<StoreData>;

    wasi::clocks::monotonic_clock::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::clocks::system_clock::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::cli::environment::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::cli::exit::add_to_linker::<_, Data>(linker, &Default::default(), |state| state)?;
    wasi::cli::stdin::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::cli::stdout::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::cli::stderr::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::cli::terminal_input::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::cli::terminal_output::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::cli::terminal_stdin::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::cli::terminal_stdout::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::cli::terminal_stderr::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::random::random::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::random::insecure::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::random::insecure_seed::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::filesystem::types::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::filesystem::preopens::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::sockets::types::add_to_linker::<_, Data>(linker, |state| state)?;
    wasi::sockets::ip_name_lookup::add_to_linker::<_, Data>(linker, |state| state)?;
    Ok(())
}

#[derive(Clone)]
struct FsNode {
    path: String,
    kind: FsNodeKind,
    contents: Vec<u8>,
    inode: u64,
    modified_nanos: u64,
    readonly: bool,
}

#[derive(Clone)]
pub struct FsDescriptor {
    pub(crate) path: String,
    pub(crate) kind: FsNodeKind,
    pub(crate) flags: fs_types::DescriptorFlags,
}

pub struct TerminalInput;
pub struct TerminalOutput;
pub struct TcpSocket;
pub struct UdpSocket;

#[repr(transparent)]
pub struct TrappableError<T> {
    err: wasmtime::Error,
    _marker: PhantomData<T>,
}

pub type FsError = TrappableError<fs_types::ErrorCode>;

impl<T> TrappableError<T> {
    fn trap(err: impl Into<wasmtime::Error>) -> Self {
        Self {
            err: err.into(),
            _marker: PhantomData,
        }
    }

    fn downcast(self) -> Result<T>
    where
        T: core::error::Error + Send + Sync + 'static,
    {
        self.err.downcast()
    }
}

impl<T> From<T> for TrappableError<T>
where
    T: core::error::Error + Send + Sync + 'static,
{
    fn from(error: T) -> Self {
        Self::trap(error)
    }
}

impl<T> core::fmt::Debug for TrappableError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.err.fmt(f)
    }
}

impl<T> core::fmt::Display for TrappableError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.err.fmt(f)
    }
}

impl<T> core::error::Error for TrappableError<T> {}

pub(crate) struct DebugFileSystem<State = crate::debug_state::RuntimeState> {
    nodes: Vec<FsNode>,
    next_inode: u64,
    runtime_state: State,
}

struct SerialStreamConsumer<T> {
    getter: fn(&mut T) -> &mut StoreData,
    stream: OutputStreamKind,
    result: Option<oneshot::Sender<core::result::Result<(), cli_types::ErrorCode>>>,
}

impl<T> SerialStreamConsumer<T> {
    fn new(
        getter: fn(&mut T) -> &mut StoreData,
        result: oneshot::Sender<core::result::Result<(), cli_types::ErrorCode>>,
        stream: OutputStreamKind,
    ) -> Self {
        Self {
            getter,
            stream,
            result: Some(result),
        }
    }

    fn complete(&mut self, result: core::result::Result<(), cli_types::ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(result);
        }
    }
}

impl<T> Drop for SerialStreamConsumer<T> {
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static> StreamConsumer<T> for SerialStreamConsumer<T> {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        let available = source.remaining(&mut store);
        if available == 0 {
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        let mut bytes = Vec::with_capacity(available);
        source.read(&mut store, &mut bytes)?;
        let consumer = self.as_ref().get_ref();
        let getter = consumer.getter;
        getter(store.data_mut()).write_output(consumer.stream, &bytes);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

enum FileWriteMode {
    At(usize),
    Append,
}

struct FileWriteConsumer<T> {
    getter: fn(&mut T) -> &mut StoreData,
    descriptor: FsDescriptor,
    mode: FileWriteMode,
    result: Option<oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>>,
}

impl<T> FileWriteConsumer<T> {
    fn new_at(
        getter: fn(&mut T) -> &mut StoreData,
        descriptor: FsDescriptor,
        offset: usize,
        result: oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>,
    ) -> Self {
        Self {
            getter,
            descriptor,
            mode: FileWriteMode::At(offset),
            result: Some(result),
        }
    }

    fn new_append(
        getter: fn(&mut T) -> &mut StoreData,
        descriptor: FsDescriptor,
        result: oneshot::Sender<core::result::Result<(), fs_types::ErrorCode>>,
    ) -> Self {
        Self {
            getter,
            descriptor,
            mode: FileWriteMode::Append,
            result: Some(result),
        }
    }

    fn complete(&mut self, result: core::result::Result<(), fs_types::ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(result);
        }
    }
}

impl<T> Drop for FileWriteConsumer<T> {
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static> StreamConsumer<T> for FileWriteConsumer<T> {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        let available = source.remaining(&mut store);
        if available == 0 {
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        let mut bytes = Vec::with_capacity(available);
        source.read(&mut store, &mut bytes)?;

        let store_data = (self.getter)(store.data_mut());
        let descriptor = self.descriptor.clone();
        let now_nanos = store_data.now_nanos();
        let write_result = match &mut self.mode {
            FileWriteMode::At(offset) => {
                let result =
                    store_data
                        .filesystem
                        .write_at(&descriptor, *offset, &bytes, now_nanos);
                if result.is_ok() {
                    *offset = offset
                        .checked_add(bytes.len())
                        .expect("file write offset overflowed usize");
                }
                result
            }
            FileWriteMode::Append => store_data.filesystem.append(&descriptor, &bytes, now_nanos),
        };

        match write_result {
            Ok(()) => Poll::Ready(Ok(StreamResult::Completed)),
            Err(error) => {
                self.complete(Err(error));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

impl<State> DebugFileSystem<State>
where
    State: helios_kernel::ComponentHostFilesystemState<crate::host_fs::HostFileSystemService>,
{
    pub(crate) fn new(runtime_state: State) -> Self {
        let mut filesystem = Self {
            nodes: vec![FsNode {
                path: String::from("/"),
                kind: FsNodeKind::Directory,
                contents: Vec::new(),
                inode: 1,
                modified_nanos: 0,
                readonly: false,
            }],
            next_inode: 2,
            runtime_state,
        };

        if let Some(init) = embedded_init() {
            filesystem.seed_bootfs(init.bootfs());
        }

        filesystem
    }

    fn seed_bootfs(&mut self, image: EmbeddedBootFs) {
        for file in image.files() {
            self.insert_bootfs_file(file);
        }
    }

    fn insert_bootfs_file(&mut self, file: &EmbeddedBootFile) {
        let absolute = format!("/{}", file.path());
        let mut current = String::from("/");

        for segment in file.path().split('/').filter(|segment| !segment.is_empty()) {
            let next = if current == "/" {
                format!("/{segment}")
            } else {
                format!("{current}/{segment}")
            };

            if next == absolute {
                break;
            }

            self.ensure_directory(&next, true);
            current = next;
        }

        if self.get_node(&absolute).is_ok() {
            return;
        }

        let inode = self.allocate_inode();
        self.nodes.push(FsNode {
            path: absolute,
            kind: FsNodeKind::File,
            contents: file.contents().to_vec(),
            inode,
            modified_nanos: 0,
            readonly: true,
        });
    }

    fn ensure_directory(&mut self, path: &str, readonly: bool) {
        if path == "/" {
            return;
        }

        if let Some(existing) = self.nodes.iter_mut().find(|node| node.path == path) {
            assert!(
                existing.kind == FsNodeKind::Directory,
                "bootfs directory path {} collided with an existing file",
                path
            );
            existing.readonly |= readonly;
            return;
        }

        let inode = self.allocate_inode();
        self.nodes.push(FsNode {
            path: path.to_owned(),
            kind: FsNodeKind::Directory,
            contents: Vec::new(),
            inode,
            modified_nanos: 0,
            readonly,
        });
    }

    fn allocate_inode(&mut self) -> u64 {
        let inode = self.next_inode;
        self.next_inode += 1;
        inode
    }

    fn host_path<'a>(&self, path: &'a str) -> Option<&'a str> {
        helios_kernel::guest_host_share_path(path)
    }

    fn host_service(
        &self,
    ) -> core::result::Result<crate::host_fs::HostFileSystemService, fs_types::ErrorCode> {
        self.runtime_state
            .host_filesystem_service()
            .ok_or(fs_types::ErrorCode::NoEntry)
    }

    pub(crate) fn root_descriptor(&self) -> FsDescriptor {
        FsDescriptor {
            path: String::from("/"),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ
                | fs_types::DescriptorFlags::WRITE
                | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
        }
    }

    fn get_node(&self, path: &str) -> core::result::Result<&FsNode, fs_types::ErrorCode> {
        self.nodes
            .iter()
            .find(|node| node.path == path)
            .ok_or(fs_types::ErrorCode::NoEntry)
    }

    fn get_node_mut(
        &mut self,
        path: &str,
    ) -> core::result::Result<&mut FsNode, fs_types::ErrorCode> {
        self.nodes
            .iter_mut()
            .find(|node| node.path == path)
            .ok_or(fs_types::ErrorCode::NoEntry)
    }

    pub(crate) fn stat(
        &self,
        path: &str,
    ) -> core::result::Result<fs_types::DescriptorStat, fs_types::ErrorCode> {
        if let Some(host_path) = self.host_path(path) {
            let metadata = helios_kernel::block_on(async {
                self.host_service()?
                    .stat_path(host_path)
                    .await
                    .map_err(map_host_fs_error)
            })?;
            return Ok(fs_types::DescriptorStat {
                type_: if metadata.qid_type & 0x80 != 0 {
                    fs_types::DescriptorType::Directory
                } else {
                    fs_types::DescriptorType::RegularFile
                },
                link_count: 1,
                size: metadata.size,
                data_access_timestamp: None,
                data_modification_timestamp: None,
                status_change_timestamp: None,
            });
        }

        let node = self.get_node(path)?;
        let size = match node.kind {
            FsNodeKind::Directory => 0,
            FsNodeKind::File => node.contents.len() as u64,
        };
        let timestamp = system_time_from_nanos(node.modified_nanos);
        Ok(fs_types::DescriptorStat {
            type_: match node.kind {
                FsNodeKind::Directory => fs_types::DescriptorType::Directory,
                FsNodeKind::File => fs_types::DescriptorType::RegularFile,
            },
            link_count: 1,
            size,
            data_access_timestamp: Some(timestamp),
            data_modification_timestamp: Some(timestamp),
            status_change_timestamp: Some(timestamp),
        })
    }

    pub(crate) fn metadata_hash(
        &self,
        path: &str,
    ) -> core::result::Result<fs_types::MetadataHashValue, fs_types::ErrorCode> {
        if let Some(host_path) = self.host_path(path) {
            let metadata = helios_kernel::block_on(async {
                self.host_service()?
                    .stat_path(host_path)
                    .await
                    .map_err(map_host_fs_error)
            })?;
            return Ok(fs_types::MetadataHashValue {
                lower: metadata.qid_path,
                upper: u64::from(metadata.mode) << 32 ^ metadata.size,
            });
        }

        let node = self.get_node(path)?;
        let size = match node.kind {
            FsNodeKind::Directory => 0,
            FsNodeKind::File => node.contents.len() as u64,
        };
        Ok(fs_types::MetadataHashValue {
            lower: node.inode,
            upper: node.modified_nanos ^ size,
        })
    }

    pub(crate) fn read_directory(
        &self,
        path: &str,
    ) -> core::result::Result<Vec<fs_types::DirectoryEntry>, fs_types::ErrorCode> {
        if let Some(host_path) = self.host_path(path) {
            let entries = helios_kernel::block_on(async {
                self.host_service()?
                    .read_dir(host_path)
                    .await
                    .map_err(map_host_fs_error)
            })?;
            return Ok(entries
                .into_iter()
                .map(|entry| fs_types::DirectoryEntry {
                    type_: if entry.is_directory {
                        fs_types::DescriptorType::Directory
                    } else {
                        fs_types::DescriptorType::RegularFile
                    },
                    name: entry.name,
                })
                .collect());
        }

        let node = self.get_node(path)?;
        if node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let prefix = helios_kernel::directory_prefix(path);
        let mut entries = Vec::new();
        for child in &self.nodes {
            if child.path == path {
                continue;
            }
            let Some(remainder) = child.path.strip_prefix(&prefix) else {
                continue;
            };
            if remainder.is_empty() {
                continue;
            }
            if let Some((name, _)) = remainder.split_once('/') {
                if entries
                    .iter()
                    .any(|entry: &fs_types::DirectoryEntry| entry.name == name)
                {
                    continue;
                }
                entries.push(fs_types::DirectoryEntry {
                    type_: fs_types::DescriptorType::Directory,
                    name: name.to_string(),
                });
                continue;
            }
            entries.push(fs_types::DirectoryEntry {
                type_: match child.kind {
                    FsNodeKind::Directory => fs_types::DescriptorType::Directory,
                    FsNodeKind::File => fs_types::DescriptorType::RegularFile,
                },
                name: remainder.to_string(),
            });
        }

        if path == "/" && self.runtime_state.host_filesystem_service().is_some() {
            let has_host_mount = entries.iter().any(|entry| entry.name == "host");
            if !has_host_mount {
                entries.push(fs_types::DirectoryEntry {
                    type_: fs_types::DescriptorType::Directory,
                    name: String::from("host"),
                });
            }
        }

        entries.sort_by(|left, right| {
            is_dir_first(left.type_.clone())
                .cmp(&is_dir_first(right.type_.clone()))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(entries)
    }

    pub(crate) fn read_file(
        &self,
        descriptor: &FsDescriptor,
        offset: u64,
    ) -> core::result::Result<Vec<u8>, fs_types::ErrorCode> {
        if let Some(host_path) = self.host_path(&descriptor.path) {
            if !descriptor.flags.contains(fs_types::DescriptorFlags::READ) {
                return Err(fs_types::ErrorCode::ReadOnly);
            }

            let bytes = helios_kernel::block_on(async {
                self.host_service()?
                    .read_file(host_path)
                    .await
                    .map_err(map_host_fs_error)
            })?;
            let offset: usize = offset
                .try_into()
                .map_err(|_| fs_types::ErrorCode::Overflow)?;
            if offset >= bytes.len() {
                return Ok(Vec::new());
            }
            return Ok(bytes[offset..].to_vec());
        }

        let node = self.get_node(&descriptor.path)?;
        if node.kind != FsNodeKind::File {
            return Err(fs_types::ErrorCode::IsDirectory);
        }
        if !descriptor.flags.contains(fs_types::DescriptorFlags::READ) {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        let offset: usize = offset
            .try_into()
            .map_err(|_| fs_types::ErrorCode::Overflow)?;
        if offset >= node.contents.len() {
            return Ok(Vec::new());
        }
        Ok(node.contents[offset..].to_vec())
    }

    pub(crate) fn write_at(
        &mut self,
        descriptor: &FsDescriptor,
        offset: usize,
        bytes: &[u8],
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if let Some(host_path) = self.host_path(&descriptor.path) {
            if descriptor.kind != FsNodeKind::File {
                return Err(fs_types::ErrorCode::IsDirectory);
            }
            if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
                return Err(fs_types::ErrorCode::ReadOnly);
            }
            let offset = u64::try_from(offset).map_err(|_| fs_types::ErrorCode::Overflow)?;
            let _ = now_nanos;
            return helios_kernel::block_on(async {
                self.host_service()?
                    .write_file(host_path, offset, bytes)
                    .await
                    .map_err(map_host_fs_error)
            });
        }

        let node = self.get_node_mut(&descriptor.path)?;
        if node.kind != FsNodeKind::File {
            return Err(fs_types::ErrorCode::IsDirectory);
        }
        if node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        let end = offset
            .checked_add(bytes.len())
            .ok_or(fs_types::ErrorCode::Overflow)?;
        if node.contents.len() < offset {
            node.contents.resize(offset, 0);
        }
        if node.contents.len() < end {
            node.contents.resize(end, 0);
        }
        node.contents[offset..end].copy_from_slice(bytes);
        node.modified_nanos = now_nanos;
        Ok(())
    }

    pub(crate) fn append(
        &mut self,
        descriptor: &FsDescriptor,
        bytes: &[u8],
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if let Some(host_path) = self.host_path(&descriptor.path) {
            if descriptor.kind != FsNodeKind::File {
                return Err(fs_types::ErrorCode::IsDirectory);
            }
            if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
                return Err(fs_types::ErrorCode::ReadOnly);
            }
            let offset = helios_kernel::block_on(async {
                self.host_service()?
                    .stat_path(host_path)
                    .await
                    .map(|metadata| metadata.size)
                    .map_err(map_host_fs_error)
            })?;
            let _ = now_nanos;
            return helios_kernel::block_on(async {
                self.host_service()?
                    .write_file(host_path, offset, bytes)
                    .await
                    .map_err(map_host_fs_error)
            });
        }

        let offset = self.get_node(&descriptor.path)?.contents.len();
        self.write_at(descriptor, offset, bytes, now_nanos)
    }

    pub(crate) fn open_at(
        &mut self,
        base: &FsDescriptor,
        path_flags: fs_types::PathFlags,
        path: &str,
        open_flags: fs_types::OpenFlags,
        descriptor_flags: fs_types::DescriptorFlags,
        now_nanos: u64,
    ) -> core::result::Result<FsDescriptor, fs_types::ErrorCode> {
        if path_flags.contains(fs_types::PathFlags::SYMLINK_FOLLOW) {
            return Err(fs_types::ErrorCode::Unsupported);
        }
        if base.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let absolute = helios_kernel::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        if let Some(host_path) = self.host_path(&absolute) {
            let metadata = helios_kernel::block_on(async {
                self.host_service()?
                    .stat_path(host_path)
                    .await
                    .map_err(map_host_fs_error)
            });

            return match metadata {
                Ok(metadata) => {
                    let kind = if metadata.qid_type & 0x80 != 0 {
                        FsNodeKind::Directory
                    } else {
                        FsNodeKind::File
                    };
                    if open_flags.contains(fs_types::OpenFlags::EXCLUSIVE)
                        && open_flags.contains(fs_types::OpenFlags::CREATE)
                    {
                        return Err(fs_types::ErrorCode::Exist);
                    }
                    if open_flags.contains(fs_types::OpenFlags::DIRECTORY)
                        && kind != FsNodeKind::Directory
                    {
                        return Err(fs_types::ErrorCode::NotDirectory);
                    }
                    if !open_flags.contains(fs_types::OpenFlags::DIRECTORY)
                        && kind == FsNodeKind::Directory
                    {
                        return Err(fs_types::ErrorCode::IsDirectory);
                    }
                    if open_flags.contains(fs_types::OpenFlags::TRUNCATE) {
                        if kind != FsNodeKind::File {
                            return Err(fs_types::ErrorCode::IsDirectory);
                        }
                        if !base
                            .flags
                            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
                        {
                            return Err(fs_types::ErrorCode::ReadOnly);
                        }
                        helios_kernel::block_on(async {
                            self.host_service()?
                                .truncate_file(host_path)
                                .await
                                .map_err(map_host_fs_error)
                        })?;
                    }
                    Ok(FsDescriptor {
                        path: absolute,
                        kind,
                        flags: descriptor_flags,
                    })
                }
                Err(fs_types::ErrorCode::NoEntry) => {
                    if !open_flags.contains(fs_types::OpenFlags::CREATE) {
                        return Err(fs_types::ErrorCode::NoEntry);
                    }
                    if open_flags.contains(fs_types::OpenFlags::DIRECTORY) {
                        return Err(fs_types::ErrorCode::Unsupported);
                    }
                    if !base
                        .flags
                        .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
                    {
                        return Err(fs_types::ErrorCode::ReadOnly);
                    }
                    helios_kernel::block_on(async {
                        self.host_service()?
                            .create_file(host_path)
                            .await
                            .map_err(map_host_fs_error)
                    })?;
                    Ok(FsDescriptor {
                        path: absolute,
                        kind: FsNodeKind::File,
                        flags: descriptor_flags,
                    })
                }
                Err(error) => Err(error),
            };
        }

        if let Ok(existing) = self.get_node_mut(&absolute) {
            if open_flags.contains(fs_types::OpenFlags::EXCLUSIVE)
                && open_flags.contains(fs_types::OpenFlags::CREATE)
            {
                return Err(fs_types::ErrorCode::Exist);
            }
            if open_flags.contains(fs_types::OpenFlags::DIRECTORY)
                && existing.kind != FsNodeKind::Directory
            {
                return Err(fs_types::ErrorCode::NotDirectory);
            }
            if !open_flags.contains(fs_types::OpenFlags::DIRECTORY)
                && existing.kind == FsNodeKind::Directory
            {
                return Err(fs_types::ErrorCode::IsDirectory);
            }
            if open_flags.contains(fs_types::OpenFlags::TRUNCATE) {
                if existing.kind != FsNodeKind::File {
                    return Err(fs_types::ErrorCode::IsDirectory);
                }
                if existing.readonly {
                    return Err(fs_types::ErrorCode::ReadOnly);
                }
                if !base
                    .flags
                    .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
                {
                    return Err(fs_types::ErrorCode::ReadOnly);
                }
                existing.contents.clear();
                existing.modified_nanos = now_nanos;
            }
            return Ok(FsDescriptor {
                path: absolute,
                kind: existing.kind,
                flags: descriptor_flags,
            });
        }

        if !open_flags.contains(fs_types::OpenFlags::CREATE) {
            return Err(fs_types::ErrorCode::NoEntry);
        }
        if open_flags.contains(fs_types::OpenFlags::DIRECTORY) {
            return Err(fs_types::ErrorCode::Unsupported);
        }
        if self.get_node(&base.path)?.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        let parent = helios_kernel::parent_path(&absolute);
        let parent_node = self.get_node(parent)?;
        if parent_node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let inode = self.allocate_inode();
        let node = FsNode {
            path: absolute.clone(),
            kind: FsNodeKind::File,
            contents: Vec::new(),
            inode,
            modified_nanos: now_nanos,
            readonly: false,
        };
        self.nodes.push(node);
        Ok(FsDescriptor {
            path: absolute,
            kind: FsNodeKind::File,
            flags: descriptor_flags,
        })
    }

    pub(crate) fn remove_directory_at(
        &mut self,
        base: &FsDescriptor,
        path: &str,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let absolute = helios_kernel::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        if let Some(host_path) = self.host_path(&absolute) {
            return helios_kernel::block_on(async {
                self.host_service()?
                    .remove(host_path, true)
                    .await
                    .map_err(map_host_fs_error)
            });
        }
        if absolute == "/" {
            return Err(fs_types::ErrorCode::NotPermitted);
        }
        let node = self.get_node(&absolute)?;
        if node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        let prefix = helios_kernel::directory_prefix(&absolute);
        if self
            .nodes
            .iter()
            .any(|child| child.path != absolute && child.path.starts_with(&prefix))
        {
            return Err(fs_types::ErrorCode::NotEmpty);
        }
        self.nodes.retain(|node| node.path != absolute);
        Ok(())
    }

    pub(crate) fn create_directory_at(
        &mut self,
        base: &FsDescriptor,
        path: &str,
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if base.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        let absolute = helios_kernel::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        if let Some(host_path) = self.host_path(&absolute) {
            let _ = now_nanos;
            return helios_kernel::block_on(async {
                self.host_service()?
                    .create_directory(host_path)
                    .await
                    .map_err(map_host_fs_error)
            });
        }
        if self.get_node(&base.path)?.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        if absolute == "/" {
            return Err(fs_types::ErrorCode::Exist);
        }
        if self.get_node(&absolute).is_ok() {
            return Err(fs_types::ErrorCode::Exist);
        }

        let parent = helios_kernel::parent_path(&absolute);
        let parent_node = self.get_node(parent)?;
        if parent_node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }

        let inode = self.allocate_inode();
        self.nodes.push(FsNode {
            path: absolute,
            kind: FsNodeKind::Directory,
            contents: Vec::new(),
            inode,
            modified_nanos: now_nanos,
            readonly: false,
        });
        Ok(())
    }

    pub(crate) fn unlink_file_at(
        &mut self,
        base: &FsDescriptor,
        path: &str,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let absolute = helios_kernel::resolve_child_path(&base.path, path).map_err(map_component_fs_path_error)?;
        if let Some(host_path) = self.host_path(&absolute) {
            return helios_kernel::block_on(async {
                self.host_service()?
                    .remove(host_path, false)
                    .await
                    .map_err(map_host_fs_error)
            });
        }
        let node = self.get_node(&absolute)?;
        if node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if node.kind != FsNodeKind::File {
            return Err(fs_types::ErrorCode::IsDirectory);
        }
        self.nodes.retain(|node| node.path != absolute);
        Ok(())
    }

    pub(crate) fn rename_at(
        &mut self,
        source_base: &FsDescriptor,
        source_path: &str,
        destination_base: &FsDescriptor,
        destination_path: &str,
        now_nanos: u64,
    ) -> core::result::Result<(), fs_types::ErrorCode> {
        if source_base.kind != FsNodeKind::Directory
            || destination_base.kind != FsNodeKind::Directory
        {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        if !source_base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
            || !destination_base
                .flags
                .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        let source_absolute = helios_kernel::resolve_child_path(&source_base.path, source_path).map_err(map_component_fs_path_error)?;
        let destination_absolute = helios_kernel::resolve_child_path(&destination_base.path, destination_path).map_err(map_component_fs_path_error)?;
        let source_host = self.host_path(&source_absolute);
        let destination_host = self.host_path(&destination_absolute);
        if source_host.is_some() || destination_host.is_some() {
            let Some(source_host) = source_host else {
                return Err(fs_types::ErrorCode::CrossDevice);
            };
            let Some(destination_host) = destination_host else {
                return Err(fs_types::ErrorCode::CrossDevice);
            };
            return helios_kernel::block_on(async {
                self.host_service()?
                    .rename(source_host, destination_host)
                    .await
                    .map_err(map_host_fs_error)
            });
        }

        if self.get_node(&source_base.path)?.readonly
            || self.get_node(&destination_base.path)?.readonly
        {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if source_absolute == "/" || destination_absolute == "/" {
            return Err(fs_types::ErrorCode::NotPermitted);
        }
        if source_absolute == destination_absolute {
            return Ok(());
        }

        let source_node = self.get_node(&source_absolute)?.clone();
        if source_node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }
        if self.get_node(&destination_absolute).is_ok() {
            return Err(fs_types::ErrorCode::Exist);
        }

        let destination_parent = helios_kernel::parent_path(&destination_absolute);
        let destination_parent_node = self.get_node(destination_parent)?;
        if destination_parent_node.kind != FsNodeKind::Directory {
            return Err(fs_types::ErrorCode::NotDirectory);
        }
        if destination_parent_node.readonly {
            return Err(fs_types::ErrorCode::ReadOnly);
        }

        if source_node.kind == FsNodeKind::Directory {
            let source_prefix = helios_kernel::directory_prefix(&source_absolute);
            if destination_absolute == source_absolute
                || destination_absolute.starts_with(&source_prefix)
            {
                return Err(fs_types::ErrorCode::NotPermitted);
            }
        }

        let source_prefix = helios_kernel::directory_prefix(&source_absolute);
        for node in &mut self.nodes {
            if node.path == source_absolute {
                node.path = destination_absolute.clone();
                node.modified_nanos = now_nanos;
                continue;
            }

            if source_node.kind == FsNodeKind::Directory && node.path.starts_with(&source_prefix) {
                node.path = format!(
                    "{}{suffix}",
                    destination_absolute,
                    suffix = &node.path[source_absolute.len()..]
                );
                node.modified_nanos = now_nanos;
            }
        }

        Ok(())
    }
}

impl wasi::clocks::monotonic_clock::Host for StoreData {
    fn now(&mut self) -> Result<wasi::clocks::monotonic_clock::Mark> {
        Ok(self.now_nanos())
    }

    fn get_resolution(&mut self) -> Result<wasi::clocks::types::Duration> {
        Ok(1)
    }
}

impl wasi::clocks::monotonic_clock::HostWithStore for HasSelf<StoreData> {
    async fn wait_until<T: Send>(
        accessor: &Accessor<T, Self>,
        when: wasi::clocks::monotonic_clock::Mark,
    ) -> Result<()> {
        while accessor.with(|mut access| access.get().now_nanos()) < when {
            core::hint::spin_loop();
        }
        Ok(())
    }

    async fn wait_for<T: Send>(
        accessor: &Accessor<T, Self>,
        duration: wasi::clocks::types::Duration,
    ) -> Result<()> {
        let deadline =
            accessor.with(|mut access| access.get().now_nanos().saturating_add(duration));
        Self::wait_until(accessor, deadline).await
    }
}

impl wasi::clocks::system_clock::Host for StoreData {
    fn now(&mut self) -> Result<wasi::clocks::system_clock::Instant> {
        Ok(system_time_from_nanos(self.now_nanos()))
    }

    fn get_resolution(&mut self) -> Result<wasi::clocks::types::Duration> {
        Ok(1)
    }
}

impl wasi::cli::environment::Host for StoreData {
    fn get_arguments(&mut self) -> Result<Vec<String>> {
        Ok(self.arguments.clone())
    }

    fn get_environment(&mut self) -> Result<Vec<(String, String)>> {
        Ok(self.environment.clone())
    }

    fn get_initial_cwd(&mut self) -> Result<Option<String>> {
        Ok(None)
    }
}

impl wasi::cli::exit::Host for StoreData {
    fn exit(&mut self, status: core::result::Result<(), ()>) -> Result<()> {
        let message = match status {
            Ok(()) => "guest requested wasi exit success",
            Err(()) => "guest requested wasi exit failure",
        };
        Err(wasmtime::Error::msg(message))
    }

    fn exit_with_code(&mut self, status_code: u8) -> Result<()> {
        Err(wasmtime::Error::msg(alloc::format!(
            "guest requested wasi exit code {status_code}"
        )))
    }
}

impl wasi::cli::stdin::Host for StoreData {}

impl wasi::cli::stdin::HostWithStore for HasSelf<StoreData> {
    fn read_via_stream<T>(
        mut access: Access<'_, T, Self>,
    ) -> Result<(
        StreamReader<u8>,
        FutureReader<core::result::Result<(), cli_types::ErrorCode>>,
    )> {
        let stream = StreamReader::new(&mut access, Vec::<u8>::new())?;
        let future = FutureReader::new(&mut access, async {
            Ok::<_, wasmtime::Error>(Ok::<(), cli_types::ErrorCode>(()))
        })?;
        Ok((stream, future))
    }
}

impl wasi::cli::stdout::Host for StoreData {}

impl wasi::cli::stdout::HostWithStore for HasSelf<StoreData> {
    fn write_via_stream<T>(
        mut access: Access<'_, T, Self>,
        data: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), cli_types::ErrorCode>>> {
        let (tx, rx) = oneshot::channel();
        let getter = access.getter();
        data.pipe(
            &mut access,
            SerialStreamConsumer::new(getter, tx, OutputStreamKind::Stdout),
        )?;
        FutureReader::new(&mut access, async move {
            match rx.await {
                Ok(result) => Ok::<_, wasmtime::Error>(result),
                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), cli_types::ErrorCode>(())),
            }
        })
    }
}

impl wasi::cli::stderr::Host for StoreData {}

impl wasi::cli::stderr::HostWithStore for HasSelf<StoreData> {
    fn write_via_stream<T>(
        mut access: Access<'_, T, Self>,
        data: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), cli_types::ErrorCode>>> {
        let (tx, rx) = oneshot::channel();
        let getter = access.getter();
        data.pipe(
            &mut access,
            SerialStreamConsumer::new(getter, tx, OutputStreamKind::Stderr),
        )?;
        FutureReader::new(&mut access, async move {
            match rx.await {
                Ok(result) => Ok::<_, wasmtime::Error>(result),
                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), cli_types::ErrorCode>(())),
            }
        })
    }
}

impl wasi::cli::terminal_input::Host for StoreData {}
impl wasi::cli::terminal_input::HostTerminalInput for StoreData {
    fn drop(&mut self, resource: Resource<TerminalInput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl wasi::cli::terminal_output::Host for StoreData {}
impl wasi::cli::terminal_output::HostTerminalOutput for StoreData {
    fn drop(&mut self, resource: Resource<TerminalOutput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl wasi::cli::terminal_stdin::Host for StoreData {
    fn get_terminal_stdin(&mut self) -> Result<Option<Resource<TerminalInput>>> {
        Ok(None)
    }
}

impl wasi::cli::terminal_stdout::Host for StoreData {
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl wasi::cli::terminal_stderr::Host for StoreData {
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl wasi::random::random::Host for StoreData {
    fn get_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(vec![0_u8; len as usize])
    }

    fn get_random_u64(&mut self) -> Result<u64> {
        Ok(0)
    }
}

impl wasi::random::insecure::Host for StoreData {
    fn get_insecure_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(vec![0_u8; len as usize])
    }

    fn get_insecure_random_u64(&mut self) -> Result<u64> {
        Ok(0)
    }
}

impl wasi::random::insecure_seed::Host for StoreData {
    fn get_insecure_seed(&mut self) -> Result<(u64, u64)> {
        Ok((0, 0))
    }
}

impl wasi::filesystem::preopens::Host for StoreData {
    fn get_directories(&mut self) -> Result<Vec<(Resource<FsDescriptor>, String)>> {
        let descriptor = self.filesystem.root_descriptor();
        let resource = self.table.push(descriptor)?;
        Ok(vec![(resource, String::from("/"))])
    }
}

impl wasi::filesystem::types::Host for StoreData {
    fn convert_error_code(&mut self, error: FsError) -> Result<fs_types::ErrorCode> {
        error.downcast()
    }
}
impl wasi::filesystem::types::HostDescriptor for StoreData {
    fn drop(&mut self, descriptor: Resource<FsDescriptor>) -> Result<()> {
        self.table.delete(descriptor)?;
        Ok(())
    }
}

impl wasi::filesystem::types::HostDescriptorWithStore for HasSelf<StoreData> {
    fn read_via_stream<T>(
        mut accessor: Access<'_, T, Self>,
        descriptor: Resource<FsDescriptor>,
        offset: u64,
    ) -> Result<(
        StreamReader<u8>,
        FutureReader<core::result::Result<(), fs_types::ErrorCode>>,
    )> {
        let bytes = {
            let descriptor = get_fs_descriptor(accessor.get(), &descriptor)?;
            accessor.get().filesystem.read_file(&descriptor, offset)
        };
        match bytes {
            Ok(bytes) => {
                let stream = StreamReader::new(&mut accessor, bytes)?;
                let future = FutureReader::new(&mut accessor, async {
                    Ok::<_, wasmtime::Error>(Ok::<(), fs_types::ErrorCode>(()))
                })?;
                Ok((stream, future))
            }
            Err(error) => {
                let stream = StreamReader::new(&mut accessor, Vec::<u8>::new())?;
                let future = FutureReader::new(&mut accessor, async move {
                    Ok::<_, wasmtime::Error>(Err::<(), fs_types::ErrorCode>(error))
                })?;
                Ok((stream, future))
            }
        }
    }

    fn write_via_stream<T>(
        mut accessor: Access<'_, T, Self>,
        descriptor: Resource<FsDescriptor>,
        mut data: StreamReader<u8>,
        offset: u64,
    ) -> Result<FutureReader<core::result::Result<(), fs_types::ErrorCode>>> {
        let offset: usize = offset
            .try_into()
            .map_err(|_| wasmtime::Error::msg("file offset does not fit into usize"))?;
        let descriptor = get_fs_descriptor(accessor.get(), &descriptor);
        let getter = accessor.getter();
        match descriptor {
            Ok(descriptor) => {
                let (tx, rx) = oneshot::channel();
                data.pipe(
                    &mut accessor,
                    FileWriteConsumer::new_at(getter, descriptor, offset, tx),
                )?;
                FutureReader::new(&mut accessor, async move {
                    match rx.await {
                        Ok(result) => Ok::<_, wasmtime::Error>(result),
                        Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), fs_types::ErrorCode>(())),
                    }
                })
            }
            Err(error) => {
                data.close(&mut accessor)?;
                FutureReader::new(&mut accessor, async move {
                    Ok::<_, wasmtime::Error>(Err::<(), fs_types::ErrorCode>(error))
                })
            }
        }
    }

    fn append_via_stream<T>(
        mut accessor: Access<'_, T, Self>,
        descriptor: Resource<FsDescriptor>,
        mut data: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), fs_types::ErrorCode>>> {
        let descriptor = get_fs_descriptor(accessor.get(), &descriptor);
        let getter = accessor.getter();
        match descriptor {
            Ok(descriptor) => {
                let (tx, rx) = oneshot::channel();
                data.pipe(
                    &mut accessor,
                    FileWriteConsumer::new_append(getter, descriptor, tx),
                )?;
                FutureReader::new(&mut accessor, async move {
                    match rx.await {
                        Ok(result) => Ok::<_, wasmtime::Error>(result),
                        Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), fs_types::ErrorCode>(())),
                    }
                })
            }
            Err(error) => {
                data.close(&mut accessor)?;
                FutureReader::new(&mut accessor, async move {
                    Ok::<_, wasmtime::Error>(Err::<(), fs_types::ErrorCode>(error))
                })
            }
        }
    }

    async fn advise<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: u64,
        _: u64,
        _: fs_types::Advice,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn sync_data<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
    ) -> Result<(), FsError> {
        Ok(())
    }

    async fn get_flags<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::DescriptorFlags, FsError> {
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            Ok(descriptor.flags)
        })
    }

    async fn get_type<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::DescriptorType, FsError> {
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            let kind = match descriptor.kind {
                FsNodeKind::Directory => fs_types::DescriptorType::Directory,
                FsNodeKind::File => fs_types::DescriptorType::RegularFile,
            };
            Ok(kind)
        })
    }

    async fn set_size<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: u64,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn set_times<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: fs_types::NewTimestamp,
        _: fs_types::NewTimestamp,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    fn read_directory<T>(
        mut accessor: Access<'_, T, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<(
        StreamReader<fs_types::DirectoryEntry>,
        FutureReader<core::result::Result<(), fs_types::ErrorCode>>,
    )> {
        let entries = {
            let descriptor = get_fs_descriptor(accessor.get(), &descriptor)?;
            accessor.get().filesystem.read_directory(&descriptor.path)
        };
        match entries {
            Ok(entries) => {
                let stream = StreamReader::new(&mut accessor, entries)?;
                let future = FutureReader::new(&mut accessor, async {
                    Ok::<_, wasmtime::Error>(Ok::<(), fs_types::ErrorCode>(()))
                })?;
                Ok((stream, future))
            }
            Err(error) => {
                let stream =
                    StreamReader::new(&mut accessor, Vec::<fs_types::DirectoryEntry>::new())?;
                let future = FutureReader::new(&mut accessor, async move {
                    Ok::<_, wasmtime::Error>(Err::<(), fs_types::ErrorCode>(error))
                })?;
                Ok((stream, future))
            }
        }
    }

    async fn sync<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
    ) -> Result<(), FsError> {
        Ok(())
    }

    async fn create_directory_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<(), FsError> {
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            let now_nanos = access.get().now_nanos();
            access
                .get()
                .filesystem
                .create_directory_at(&descriptor, &path, now_nanos)
                .map_err(Into::into)
        })
    }

    async fn stat<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::DescriptorStat, FsError> {
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            let path = descriptor.path;
            access.get().filesystem.stat(&path).map_err(Into::into)
        })
    }

    async fn stat_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path_flags: fs_types::PathFlags,
        path: String,
    ) -> Result<fs_types::DescriptorStat, FsError> {
        if path_flags.contains(fs_types::PathFlags::SYMLINK_FOLLOW) {
            return Err(fs_types::ErrorCode::Unsupported.into());
        }
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            let absolute = helios_kernel::resolve_child_path(&descriptor.path, &path).map_err(map_component_fs_path_error)?;
            access.get().filesystem.stat(&absolute).map_err(Into::into)
        })
    }

    async fn set_times_at<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: fs_types::PathFlags,
        _: String,
        _: fs_types::NewTimestamp,
        _: fs_types::NewTimestamp,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn link_at<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: fs_types::PathFlags,
        _: String,
        _: Resource<FsDescriptor>,
        _: String,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn open_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path_flags: fs_types::PathFlags,
        path: String,
        open_flags: fs_types::OpenFlags,
        flags: fs_types::DescriptorFlags,
    ) -> Result<Resource<FsDescriptor>, FsError> {
        accessor.with(|mut access| {
            let base = get_fs_descriptor(access.get(), &descriptor)?;
            let now_nanos = access.get().now_nanos();
            let opened = access
                .get()
                .filesystem
                .open_at(&base, path_flags, &path, open_flags, flags, now_nanos)
                .map_err(FsError::from)?;
            let resource = access.get().table.push(opened).map_err(FsError::trap)?;
            Ok(resource)
        })
    }

    async fn readlink_at<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: String,
    ) -> Result<String, FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn remove_directory_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<(), FsError> {
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            access
                .get()
                .filesystem
                .remove_directory_at(&descriptor, &path)
                .map_err(Into::into)
        })
    }

    async fn rename_at<T: Send>(
        accessor: &Accessor<T, Self>,
        source_descriptor: Resource<FsDescriptor>,
        source_path: String,
        destination_descriptor: Resource<FsDescriptor>,
        destination_path: String,
    ) -> Result<(), FsError> {
        accessor.with(|mut access| {
            let source_base = get_fs_descriptor(access.get(), &source_descriptor)?;
            let destination_base = get_fs_descriptor(access.get(), &destination_descriptor)?;
            let now_nanos = access.get().now_nanos();
            access
                .get()
                .filesystem
                .rename_at(
                    &source_base,
                    &source_path,
                    &destination_base,
                    &destination_path,
                    now_nanos,
                )
                .map_err(Into::into)
        })
    }

    async fn symlink_at<T: Send>(
        _: &Accessor<T, Self>,
        _: Resource<FsDescriptor>,
        _: String,
        _: String,
    ) -> Result<(), FsError> {
        Err(fs_types::ErrorCode::Unsupported.into())
    }

    async fn unlink_file_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path: String,
    ) -> Result<(), FsError> {
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            access
                .get()
                .filesystem
                .unlink_file_at(&descriptor, &path)
                .map_err(Into::into)
        })
    }

    async fn is_same_object<T: Send>(
        accessor: &Accessor<T, Self>,
        a: Resource<FsDescriptor>,
        b: Resource<FsDescriptor>,
    ) -> Result<bool> {
        accessor.with(|mut access| {
            let left = access.get().table.get(&a)?.clone();
            let right = access.get().table.get(&b)?.clone();
            Ok(left.path == right.path && left.kind == right.kind)
        })
    }

    async fn metadata_hash<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
    ) -> Result<fs_types::MetadataHashValue, FsError> {
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            let path = descriptor.path;
            access
                .get()
                .filesystem
                .metadata_hash(&path)
                .map_err(Into::into)
        })
    }

    async fn metadata_hash_at<T: Send>(
        accessor: &Accessor<T, Self>,
        descriptor: Resource<FsDescriptor>,
        path_flags: fs_types::PathFlags,
        path: String,
    ) -> Result<fs_types::MetadataHashValue, FsError> {
        if path_flags.contains(fs_types::PathFlags::SYMLINK_FOLLOW) {
            return Err(fs_types::ErrorCode::Unsupported.into());
        }
        accessor.with(|mut access| {
            let descriptor = get_fs_descriptor(access.get(), &descriptor)?;
            let absolute = helios_kernel::resolve_child_path(&descriptor.path, &path).map_err(map_component_fs_path_error)?;
            access
                .get()
                .filesystem
                .metadata_hash(&absolute)
                .map_err(Into::into)
        })
    }
}

impl wasi::sockets::types::Host for StoreData {}
impl wasi::sockets::types::HostTcpSocket for StoreData {
    async fn bind(
        &mut self,
        _: Resource<TcpSocket>,
        _: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn create(
        &mut self,
        _: socket_types::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<TcpSocket>, socket_types::ErrorCode>> {
        Ok(Err(socket_types::ErrorCode::NotSupported))
    }

    fn get_local_address(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_remote_address(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_is_listening(&mut self, _: Resource<TcpSocket>) -> Result<bool> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_address_family(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<socket_types::IpAddressFamily> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_listen_backlog_size(
        &mut self,
        _: Resource<TcpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_keep_alive_enabled(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<bool, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_keep_alive_enabled(
        &mut self,
        _: Resource<TcpSocket>,
        _: bool,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_keep_alive_idle_time(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<wasi::clocks::types::Duration, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_keep_alive_idle_time(
        &mut self,
        _: Resource<TcpSocket>,
        _: wasi::clocks::types::Duration,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_keep_alive_interval(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<wasi::clocks::types::Duration, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_keep_alive_interval(
        &mut self,
        _: Resource<TcpSocket>,
        _: wasi::clocks::types::Duration,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_keep_alive_count(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u32, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_keep_alive_count(
        &mut self,
        _: Resource<TcpSocket>,
        _: u32,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_hop_limit(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u8, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_hop_limit(
        &mut self,
        _: Resource<TcpSocket>,
        _: u8,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_receive_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_receive_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn get_send_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn set_send_buffer_size(
        &mut self,
        _: Resource<TcpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn drop(&mut self, resource: Resource<TcpSocket>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl wasi::sockets::types::HostUdpSocket for StoreData {
    async fn bind(
        &mut self,
        _: Resource<UdpSocket>,
        _: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    async fn connect(
        &mut self,
        _: Resource<UdpSocket>,
        _: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn create(
        &mut self,
        _: socket_types::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<UdpSocket>, socket_types::ErrorCode>> {
        Ok(Err(socket_types::ErrorCode::NotSupported))
    }

    fn disconnect(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_local_address(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_remote_address(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_address_family(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<socket_types::IpAddressFamily> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_unicast_hop_limit(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u8, socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn set_unicast_hop_limit(
        &mut self,
        _: Resource<UdpSocket>,
        _: u8,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_receive_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn set_receive_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn get_send_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn set_send_buffer_size(
        &mut self,
        _: Resource<UdpSocket>,
        _: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    fn drop(&mut self, resource: Resource<UdpSocket>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl wasi::sockets::types::HostTcpSocketWithStore for HasSelf<StoreData> {
    async fn connect<T>(
        _: &Accessor<T, Self>,
        _: Resource<TcpSocket>,
        _: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn listen<T>(
        _: Access<'_, T, Self>,
        _: Resource<TcpSocket>,
    ) -> Result<core::result::Result<StreamReader<Resource<TcpSocket>>, socket_types::ErrorCode>>
    {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn send<T>(
        _: Access<'_, T, Self>,
        _: Resource<TcpSocket>,
        _: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), socket_types::ErrorCode>>> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }

    fn receive<T>(
        _: Access<'_, T, Self>,
        _: Resource<TcpSocket>,
    ) -> Result<(
        StreamReader<u8>,
        FutureReader<core::result::Result<(), socket_types::ErrorCode>>,
    )> {
        unreachable!("tcp sockets are not available on the embedded debugger host")
    }
}

impl wasi::sockets::types::HostUdpSocketWithStore for HasSelf<StoreData> {
    async fn send<T>(
        _: &Accessor<T, Self>,
        _: Resource<UdpSocket>,
        _: Vec<u8>,
        _: Option<socket_types::IpSocketAddress>,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }

    async fn receive<T>(
        _: &Accessor<T, Self>,
        _: Resource<UdpSocket>,
    ) -> Result<
        core::result::Result<(Vec<u8>, socket_types::IpSocketAddress), socket_types::ErrorCode>,
    > {
        unreachable!("udp sockets are not available on the embedded debugger host")
    }
}

impl wasi::sockets::ip_name_lookup::Host for StoreData {}

impl wasi::sockets::ip_name_lookup::HostWithStore for HasSelf<StoreData> {
    async fn resolve_addresses<T: Send>(
        _: &Accessor<T, Self>,
        _: String,
    ) -> Result<core::result::Result<Vec<socket_types::IpAddress>, ip_name_lookup::ErrorCode>> {
        Ok(Err(ip_name_lookup::ErrorCode::Other(Some(String::from(
            "ip-name-lookup is unsupported on the embedded debugger host",
        )))))
    }
}

fn system_time_from_nanos(nanos: u64) -> wasi::clocks::system_clock::Instant {
    let seconds = nanos / 1_000_000_000;
    let nanoseconds = (nanos % 1_000_000_000) as u32;
    wasi::clocks::system_clock::Instant {
        seconds: seconds
            .try_into()
            .expect("debugger wall clock exceeded wasi system clock range"),
        nanoseconds,
    }
}

fn get_fs_descriptor(
    store: &mut StoreData,
    resource: &Resource<FsDescriptor>,
) -> core::result::Result<FsDescriptor, fs_types::ErrorCode> {
    store
        .table
        .get(resource)
        .cloned()
        .map_err(fs_resource_error)
}

pub(crate) fn fs_resource_error(error: helios_kernel::wasmtime::component::ResourceTableError) -> fs_types::ErrorCode {
    match helios_kernel::map_resource_table_error(error) {
        helios_kernel::ComponentFsResourceError::BadDescriptor => fs_types::ErrorCode::BadDescriptor,
        helios_kernel::ComponentFsResourceError::Busy => fs_types::ErrorCode::Busy,
        helios_kernel::ComponentFsResourceError::Overflow => fs_types::ErrorCode::Overflow,
    }
}

fn is_dir_first(kind: fs_types::DescriptorType) -> u8 {
    match kind {
        fs_types::DescriptorType::Directory => 0,
        _ => 1,
    }
}

fn map_component_fs_path_error(error: helios_kernel::ComponentFsPathError) -> fs_types::ErrorCode {
    match error {
        helios_kernel::ComponentFsPathError::InvalidBasePath => fs_types::ErrorCode::Invalid,
        helios_kernel::ComponentFsPathError::NotPermitted => fs_types::ErrorCode::NotPermitted,
    }
}

fn map_host_fs_error(error: helios_kernel::HostFsError) -> fs_types::ErrorCode {
    match error {
        helios_kernel::HostFsError::Transport(IoError::NotFound) => fs_types::ErrorCode::NoEntry,
        helios_kernel::HostFsError::Transport(IoError::AlreadyExists) => {
            fs_types::ErrorCode::Exist
        }
        helios_kernel::HostFsError::Transport(IoError::NotDirectory) => {
            fs_types::ErrorCode::NotDirectory
        }
        helios_kernel::HostFsError::Transport(IoError::IsDirectory) => {
            fs_types::ErrorCode::IsDirectory
        }
        helios_kernel::HostFsError::Transport(IoError::DirectoryNotEmpty) => {
            fs_types::ErrorCode::NotEmpty
        }
        helios_kernel::HostFsError::Transport(IoError::PermissionDenied)
        | helios_kernel::HostFsError::Transport(IoError::ReadOnly) => {
            fs_types::ErrorCode::ReadOnly
        }
        helios_kernel::HostFsError::Transport(IoError::Unsupported) => {
            fs_types::ErrorCode::Unsupported
        }
        helios_kernel::HostFsError::Transport(IoError::InvalidBufferLength { .. })
        | helios_kernel::HostFsError::Transport(IoError::InvalidDeviceConfig(_))
        | helios_kernel::HostFsError::Transport(IoError::OutOfBounds)
        | helios_kernel::HostFsError::Transport(IoError::DeviceFault)
        | helios_kernel::HostFsError::Protocol(_)
        | helios_kernel::HostFsError::Utf8 => fs_types::ErrorCode::Io,
        helios_kernel::HostFsError::Server(code) => match code {
            2 => fs_types::ErrorCode::NoEntry,
            17 => fs_types::ErrorCode::Exist,
            20 => fs_types::ErrorCode::NotDirectory,
            21 => fs_types::ErrorCode::IsDirectory,
            39 => fs_types::ErrorCode::NotEmpty,
            _ => fs_types::ErrorCode::Io,
        },
    }
}

#[cfg(test)]
mod tests {
    use helios_kernel::{EmbeddedBootFile, EmbeddedBootFs};

    use super::{DebugFileSystem, FsNodeKind, bindings};

    #[test]
    fn create_directory_adds_node() {
        let mut filesystem = DebugFileSystem::new();
        let root = filesystem.root_descriptor();

        filesystem
            .create_directory_at(&root, "tmp", 7)
            .expect("directory creation must succeed");

        let node = filesystem
            .get_node("/tmp")
            .expect("directory node must exist after creation");
        assert_eq!(node.kind, FsNodeKind::Directory);
        assert_eq!(node.modified_nanos, 7);
    }

    #[test]
    fn create_directory_rejects_existing_path() {
        let mut filesystem = DebugFileSystem::new();
        let root = filesystem.root_descriptor();

        filesystem
            .create_directory_at(&root, "tmp", 1)
            .expect("initial directory creation must succeed");

        let error = filesystem
            .create_directory_at(&root, "tmp", 2)
            .expect_err("creating the same directory twice must fail");
        assert_eq!(error, bindings::wasi::filesystem::types::ErrorCode::Exist);
    }

    #[test]
    fn bootfs_seed_adds_readonly_programs() {
        let mut filesystem = DebugFileSystem::new();
        let bootfs = EmbeddedBootFs::new(&[EmbeddedBootFile::new("bin/tool", b"tool")]);
        filesystem.seed_bootfs(bootfs);

        let program = filesystem
            .get_node("/bin/tool")
            .expect("bootfs program must be present");
        assert_eq!(program.kind, FsNodeKind::File);
        assert!(program.readonly);

        let directory = filesystem
            .get_node("/bin")
            .expect("bootfs program directory must be present");
        assert_eq!(directory.kind, FsNodeKind::Directory);
        assert!(directory.readonly);
    }

    #[test]
    fn readonly_bootfs_file_rejects_writes() {
        let mut filesystem = DebugFileSystem::new();
        let bootfs = EmbeddedBootFs::new(&[EmbeddedBootFile::new("bin/tool", b"tool")]);
        filesystem.seed_bootfs(bootfs);
        let root = filesystem.root_descriptor();

        let descriptor = filesystem
            .open_at(
                &root,
                bindings::wasi::filesystem::types::PathFlags::empty(),
                "bin/tool",
                bindings::wasi::filesystem::types::OpenFlags::empty(),
                bindings::wasi::filesystem::types::DescriptorFlags::WRITE,
                0,
            )
            .expect("opening readonly bootfs file must succeed for lookup");

        let error = filesystem
            .write_at(&descriptor, 0, b"x", 1)
            .expect_err("readonly bootfs file must reject writes");
        assert_eq!(
            error,
            bindings::wasi::filesystem::types::ErrorCode::ReadOnly
        );
    }

    #[test]
    fn opening_existing_directory_without_directory_flag_fails() {
        let mut filesystem = DebugFileSystem::new();
        let root = filesystem.root_descriptor();

        filesystem
            .create_directory_at(&root, "tmp", 1)
            .expect("directory creation must succeed");

        let error = filesystem
            .open_at(
                &root,
                bindings::wasi::filesystem::types::PathFlags::empty(),
                "tmp",
                bindings::wasi::filesystem::types::OpenFlags::CREATE,
                bindings::wasi::filesystem::types::DescriptorFlags::WRITE,
                2,
            )
            .expect_err("opening an existing directory as a file must fail");
        assert_eq!(
            error,
            bindings::wasi::filesystem::types::ErrorCode::IsDirectory
        );
    }
}
