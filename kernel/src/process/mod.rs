//! Process, thread, descriptor, and authority subsystem.
//!
//! `table` and `thread` track live processes and their threads.
//! `authority` declares the capability rights granted to a process.
//! `descriptor` is the per-process file/handle table. `futex` backs
//! guest thread futex wait/wake. `program_service` exposes the
//! program execution status types used by both the runtime backend
//! and the kernel-facing API.

mod authority;
mod descriptor;
mod futex;
mod program_service;
mod table;
mod thread;

pub use authority::{
    ClockAuthorityRights, DirectoryAuthorityRights, DirectoryCap, DirectoryPreopen, DnsCap,
    ExecAuthority, ForkAuthority, JoinAuthority, LinkAuthorityRights, LinkSourceCap,
    LinkTargetDirectoryCap, MulticastCap, NetworkAdminCap, NetworkAuthorityRights, NetworkCap,
    PrivilegedBindCap, ProcessAuthority, ProcessAuthorityError, ProcessAuthorityRights,
    SetWallClockCap, SignalAuthority, SpawnAuthority, SymlinkCreateCap, SymlinkReadCap, TcpCap,
    TerminalAuthorityRights, TerminalInputCap, TerminalOutputCap, TtyControlCap, UdpCap,
};
pub(crate) use descriptor::FreeDescriptorSlots;
pub use descriptor::{DescriptorEntry, DescriptorId, DescriptorTable, DescriptorTableError};
pub use futex::{FutexKey, FutexTable, FutexWaitRegistration, GuestAddress, ProcessMemoryIdentity};
pub use program_service::{
    ProgramExecError, ProgramExecErrorDetail, ProgramExecErrorKind, ProgramOutOfMemory,
};
pub use table::{ProcessId, ProcessRecord, ProcessState, ProcessTable, ProcessTableError};
pub use thread::{ThreadId, ThreadRecord, ThreadState, ThreadTable, ThreadTableError};
