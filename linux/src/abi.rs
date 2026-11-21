//! Where the Linux ABI keeps its values in the architectural registers and
//! which signal a processor fault becomes, per instruction set.

use helios_hal::hv::RegisterId;
use helios_hal::native::{FaultKind, InstructionSet};

/// A Linux ABI this crate decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abi {
    X86_64,
    Aarch64,
}

/// The system calls the decoder serves, independent of their numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Read,
    Write,
    Close,
    Pipe,
    Pipe2,
    Dup,
    Dup2,
    Dup3,
    Fcntl,
    Eventfd,
    Eventfd2,
    EpollCreate,
    EpollCreate1,
    EpollCtl,
    EpollWait,
    EpollPwait,
    Mmap,
    Munmap,
    Mprotect,
    Mremap,
    Madvise,
    Brk,
    Ioctl,
    Writev,
    SchedYield,
    SchedGetaffinity,
    Nanosleep,
    ClockNanosleep,
    Clone,
    Clone3,
    Futex,
    SetRobustList,
    GetRobustList,
    Getpid,
    Gettid,
    SetTidAddress,
    ArchPrctl,
    Exit,
    ExitGroup,
    ClockGettime,
    ClockGetres,
    ClockSettime,
    Gettimeofday,
    /// x86-64 only: aarch64's asm-generic table has no `time`.
    Time,
    Getrandom,
    Uname,
    Sysinfo,
    Getuid,
    Geteuid,
    Getgid,
    Getegid,
    Getgroups,
    Getresuid,
    Getresgid,
    Prlimit64,
    Getrlimit,
    Setrlimit,
    Umask,
    Getpriority,
    Setpriority,
    Unknown,
}

/// Decoded arguments of the legacy `clone` system call, whose `tls` and
/// `child_tid` positions differ between x86-64 and `CONFIG_CLONE_BACKWARDS`
/// architectures such as AArch64 (`kernel/fork.c` `SYSCALL_DEFINE5(clone)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloneRegisters {
    pub flags: u64,
    pub stack: u64,
    pub parent_tid: u64,
    pub child_tid: u64,
    pub tls: u64,
}

impl Abi {
    /// The Linux ABI of `isa`, or `None` where this crate decodes none.
    pub fn for_instruction_set(isa: InstructionSet) -> Option<Self> {
        match isa {
            InstructionSet::X86_64 => Some(Self::X86_64),
            InstructionSet::Aarch64 => Some(Self::Aarch64),
            InstructionSet::Riscv64 => None,
        }
    }

    /// The operation system call `number` names.
    pub fn operation(self, number: u64) -> Operation {
        match self {
            // arch/x86/entry/syscalls/syscall_64.tbl, Linux v6.12.
            Self::X86_64 => match number {
                0 => Operation::Read,
                1 => Operation::Write,
                3 => Operation::Close,
                9 => Operation::Mmap,
                10 => Operation::Mprotect,
                11 => Operation::Munmap,
                12 => Operation::Brk,
                16 => Operation::Ioctl,
                20 => Operation::Writev,
                22 => Operation::Pipe,
                24 => Operation::SchedYield,
                25 => Operation::Mremap,
                28 => Operation::Madvise,
                32 => Operation::Dup,
                33 => Operation::Dup2,
                35 => Operation::Nanosleep,
                39 => Operation::Getpid,
                56 => Operation::Clone,
                60 => Operation::Exit,
                63 => Operation::Uname,
                72 => Operation::Fcntl,
                95 => Operation::Umask,
                96 => Operation::Gettimeofday,
                97 => Operation::Getrlimit,
                99 => Operation::Sysinfo,
                102 => Operation::Getuid,
                104 => Operation::Getgid,
                107 => Operation::Geteuid,
                108 => Operation::Getegid,
                115 => Operation::Getgroups,
                118 => Operation::Getresuid,
                120 => Operation::Getresgid,
                140 => Operation::Getpriority,
                141 => Operation::Setpriority,
                158 => Operation::ArchPrctl,
                160 => Operation::Setrlimit,
                186 => Operation::Gettid,
                201 => Operation::Time,
                202 => Operation::Futex,
                204 => Operation::SchedGetaffinity,
                213 => Operation::EpollCreate,
                218 => Operation::SetTidAddress,
                227 => Operation::ClockSettime,
                228 => Operation::ClockGettime,
                229 => Operation::ClockGetres,
                230 => Operation::ClockNanosleep,
                231 => Operation::ExitGroup,
                232 => Operation::EpollWait,
                233 => Operation::EpollCtl,
                273 => Operation::SetRobustList,
                274 => Operation::GetRobustList,
                281 => Operation::EpollPwait,
                284 => Operation::Eventfd,
                290 => Operation::Eventfd2,
                291 => Operation::EpollCreate1,
                292 => Operation::Dup3,
                293 => Operation::Pipe2,
                302 => Operation::Prlimit64,
                318 => Operation::Getrandom,
                435 => Operation::Clone3,
                _ => Operation::Unknown,
            },
            // include/uapi/asm-generic/unistd.h, which has none of the
            // legacy calls (pipe, dup2, epoll_create, epoll_wait, eventfd,
            // time) and no arch_prctl: user code writes TPIDR_EL0 itself.
            Self::Aarch64 => match number {
                19 => Operation::Eventfd2,
                20 => Operation::EpollCreate1,
                21 => Operation::EpollCtl,
                22 => Operation::EpollPwait,
                23 => Operation::Dup,
                24 => Operation::Dup3,
                25 => Operation::Fcntl,
                29 => Operation::Ioctl,
                57 => Operation::Close,
                59 => Operation::Pipe2,
                63 => Operation::Read,
                64 => Operation::Write,
                66 => Operation::Writev,
                93 => Operation::Exit,
                94 => Operation::ExitGroup,
                96 => Operation::SetTidAddress,
                98 => Operation::Futex,
                99 => Operation::SetRobustList,
                100 => Operation::GetRobustList,
                101 => Operation::Nanosleep,
                112 => Operation::ClockSettime,
                113 => Operation::ClockGettime,
                114 => Operation::ClockGetres,
                115 => Operation::ClockNanosleep,
                123 => Operation::SchedGetaffinity,
                124 => Operation::SchedYield,
                140 => Operation::Setpriority,
                141 => Operation::Getpriority,
                148 => Operation::Getresuid,
                150 => Operation::Getresgid,
                158 => Operation::Getgroups,
                160 => Operation::Uname,
                163 => Operation::Getrlimit,
                164 => Operation::Setrlimit,
                166 => Operation::Umask,
                169 => Operation::Gettimeofday,
                172 => Operation::Getpid,
                174 => Operation::Getuid,
                175 => Operation::Geteuid,
                176 => Operation::Getgid,
                177 => Operation::Getegid,
                178 => Operation::Gettid,
                179 => Operation::Sysinfo,
                214 => Operation::Brk,
                215 => Operation::Munmap,
                216 => Operation::Mremap,
                220 => Operation::Clone,
                222 => Operation::Mmap,
                226 => Operation::Mprotect,
                233 => Operation::Madvise,
                261 => Operation::Prlimit64,
                278 => Operation::Getrandom,
                435 => Operation::Clone3,
                _ => Operation::Unknown,
            },
        }
    }

    /// The registers a system call is read from and answered in.
    pub fn system_call_registers(self) -> SystemCallRegisters {
        match self {
            // arch/x86/entry/entry_64.S: RAX holds the number and the
            // result, RDI, RSI, RDX, R10, R8 and R9 the arguments
            // (encodings 0, 7, 6, 2, 10, 8, 9).
            Self::X86_64 => SystemCallRegisters {
                number: RegisterId::X86Gpr(0),
                arguments: [
                    RegisterId::X86Gpr(7),
                    RegisterId::X86Gpr(6),
                    RegisterId::X86Gpr(2),
                    RegisterId::X86Gpr(10),
                    RegisterId::X86Gpr(8),
                    RegisterId::X86Gpr(9),
                ],
                result: RegisterId::X86Gpr(0),
            },
            // arch/arm64/kernel/syscall.c: x8 holds the number, x0 to x5
            // the arguments, x0 the result.
            Self::Aarch64 => SystemCallRegisters {
                number: RegisterId::Aarch64X(8),
                arguments: [
                    RegisterId::Aarch64X(0),
                    RegisterId::Aarch64X(1),
                    RegisterId::Aarch64X(2),
                    RegisterId::Aarch64X(3),
                    RegisterId::Aarch64X(4),
                    RegisterId::Aarch64X(5),
                ],
                result: RegisterId::Aarch64X(0),
            },
        }
    }

    /// The user stack-pointer register of this ABI.
    pub fn stack_pointer_register(self) -> RegisterId {
        match self {
            Self::X86_64 => RegisterId::X86Gpr(4),
            Self::Aarch64 => RegisterId::Aarch64Sp,
        }
    }

    /// Decodes the register order of `clone(2)` (`kernel/fork.c`
    /// `SYSCALL_DEFINE5(clone)`): `CONFIG_CLONE_BACKWARDS` on arm64 swaps
    /// `tls` and `child_tid` relative to x86-64.
    pub fn clone_registers(self, args: [u64; 6]) -> CloneRegisters {
        match self {
            Self::X86_64 => CloneRegisters {
                flags: args[0],
                stack: args[1],
                parent_tid: args[2],
                child_tid: args[3],
                tls: args[4],
            },
            Self::Aarch64 => CloneRegisters {
                flags: args[0],
                stack: args[1],
                parent_tid: args[2],
                tls: args[3],
                child_tid: args[4],
            },
        }
    }

    /// Where `struct epoll_event`'s 64-bit data sits after its 32-bit
    /// events: x86-64 packs the struct (uapi/linux/eventpoll.h,
    /// `EPOLL_PACKED`), every other ABI aligns the data to 8. The struct
    /// is the offset plus 8 bytes long.
    pub fn epoll_data_offset(self) -> usize {
        match self {
            Self::X86_64 => 4,
            Self::Aarch64 => 8,
        }
    }

    /// Linux's `TASK_SIZE` for this ABI's processes: the end of the
    /// addresses a mapping may reach. x86-64 keeps the last page below the
    /// canonical hole unmappable (`TASK_SIZE_MAX`, (1 << 47) - PAGE_SIZE in
    /// arch/x86/include/asm/page_64_types.h); arm64 ends at 1 << VA_BITS
    /// (`TASK_SIZE_64`, arch/arm64/include/asm/processor.h) with the 48-bit
    /// user range Helios's TTBR0 provides.
    pub fn task_size(self) -> u64 {
        match self {
            Self::X86_64 => (1 << 47) - 4096,
            Self::Aarch64 => 1 << 48,
        }
    }

    /// The ELF `e_machine` of this ABI's executables (System V gABI).
    pub fn elf_machine(self) -> u16 {
        match self {
            Self::X86_64 => 62,
            Self::Aarch64 => 183,
        }
    }
}

/// The registers a system call is read from and answered in.
#[derive(Clone, Copy, Debug)]
pub struct SystemCallRegisters {
    pub number: RegisterId,
    pub arguments: [RegisterId; 6],
    pub result: RegisterId,
}

// Linux uapi/asm-generic/signal.h numbers, shared by x86-64.
pub const SIGILL: u8 = 4;
pub const SIGTRAP: u8 = 5;
pub const SIGABRT: u8 = 6;
pub const SIGBUS: u8 = 7;
pub const SIGFPE: u8 = 8;
pub const SIGKILL: u8 = 9;
pub const SIGSEGV: u8 = 11;

/// The signal Linux raises for a user fault of `kind`
/// (arch/x86/kernel/traps.c and arch/x86/mm/fault.c).
pub fn fault_signal(kind: FaultKind) -> u8 {
    match kind {
        FaultKind::Page { .. } | FaultKind::Protection => SIGSEGV,
        FaultKind::InvalidInstruction => SIGILL,
        FaultKind::Arithmetic => SIGFPE,
        FaultKind::Breakpoint => SIGTRAP,
        // arch/arm64/kernel/traps.c and arch/arm64/mm/fault.c (`do_sea`).
        FaultKind::Alignment | FaultKind::Bus => SIGBUS,
    }
}

/// Encodes `termination` into the 16-bit status word `wait4(2)` writes
/// (`kernel/exit.c` `wait_task_zombie`, `include/uapi/linux/wait.h`):
/// `(code & 0xff) << 8` on exit, `(signal & 0x7f) | (core_dumped << 7)`
/// on signal death, `SIGABRT` on a trap, and `SIGKILL` on an owner kill.
pub fn wait_status(termination: helios_unix::ChildTermination) -> i32 {
    match termination {
        helios_unix::ChildTermination::Exited(code) => ((code & 0xff) as i32) << 8,
        helios_unix::ChildTermination::Signaled(helios_unix::SignalTermination {
            signal,
            core_dumped,
        }) => i32::from(signal & 0x7f) | (i32::from(core_dumped) << 7),
        helios_unix::ChildTermination::Trapped => i32::from(SIGABRT),
        helios_unix::ChildTermination::Killed => i32::from(SIGKILL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_unix::{ChildTermination, SignalTermination};

    #[test]
    fn wait_status_encodes_each_termination_kind() {
        assert_eq!(wait_status(ChildTermination::Exited(0)), 0);
        assert_eq!(wait_status(ChildTermination::Exited(42)), 42 << 8);
        assert_eq!(wait_status(ChildTermination::Exited(0x1ff)), 0xff << 8);
        assert_eq!(
            wait_status(ChildTermination::Signaled(SignalTermination {
                signal: SIGSEGV,
                core_dumped: false,
            })),
            i32::from(SIGSEGV),
        );
        assert_eq!(
            wait_status(ChildTermination::Signaled(SignalTermination {
                signal: SIGSEGV,
                core_dumped: true,
            })),
            i32::from(SIGSEGV) | 0x80,
        );
        assert_eq!(wait_status(ChildTermination::Trapped), i32::from(SIGABRT));
        assert_eq!(wait_status(ChildTermination::Killed), i32::from(SIGKILL));
    }
}
