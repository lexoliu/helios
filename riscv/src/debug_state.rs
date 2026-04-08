pub(crate) type RuntimeState = helios_kernel::RuntimeState<
    crate::program_host::UserProgramService,
    crate::net::NetworkService,
    crate::host_fs::HostFileSystemService,
>;
