//! Native user execution: code compiled for the processor's own instruction
//! set, run at its lowest privilege level in an address space of its own.
//!
//! The contract names hardware properties only: isolated user address
//! spaces, a user register context that runs until the processor leaves
//! user mode, and the reason it left. Which userspace ABI the code speaks
//! (Linux's syscall numbers, its errno values, its signal frames) is the
//! consumer's business, decoded from the architectural registers this
//! contract exposes.
//!
//! # Consumers
//!
//! The kernel's Linux process host is the first: it maps an ELF image,
//! builds the initial stack, resumes a context until it stops, and answers
//! each system call, `brk`, `munmap`, `mprotect`, `mremap` and `madvise`
//! among them through the space's removal, protection, move and discard
//! operations. The shape also carries what comes next on the same surface:
//! several threads of one process running on several processors in one
//! space, whose concurrent removals the slice count and shootdown below
//! already cover; signal delivery that saves and restores the complete
//! register and extended state; and copy-on-write duplication of a space,
//! which narrows every page with [`UserSpace::protect`]'s shootdown and
//! answers the write faults that follow through
//! [`UserSpace::resolve_fault`].
//!
//! # Concurrency contract
//!
//! - A [`UserSpace`] is shared: every thread of a process resumes its own
//!   context against the same space, on any processor, at the same time.
//!   Every operation takes `&self`. Operations that change the page tables
//!   serialise on a per-space spin lock held for the walk alone, never
//!   across an `.await` and never reached from interrupt context.
//! - Mapping only makes not-present pages present, which no processor can
//!   have cached, so it needs no shootdown and is visible to every slice
//!   that starts after it returns. An operation that removes a translation
//!   or narrows its permissions must invalidate it on every processor that
//!   has run a slice of the space since the translation was installed, and
//!   free nothing it removed until each of them has acknowledged.
//! - Every backend's slice exit leaves no translation of the space usable
//!   on its processor, so the processors that can hold one are exactly
//!   those running a slice of the space at that moment. A space counts its
//!   running slices: a slice is counted before it loads the space's root
//!   and uncounted after its exit has dropped the translations, and a
//!   removal reads the count after its table writes are visible. A
//!   removal that finds no slice running needs no remote invalidation; one
//!   that finds any runs the backend's own shootdown (an IPI broadcast and
//!   its acknowledgements, or a broadcast TLB maintenance instruction and
//!   the barrier that completes it) before it returns.
//! - [`UserSpace::unmap`], [`UserSpace::discard`], a narrowing
//!   [`UserSpace::protect`] and [`UserSpace::remap`] remove or narrow
//!   translations. Each holds the space's lock from its first table write
//!   through the shootdown, then frees the frames it removed; the lock is
//!   never taken with interrupts masked, so a processor spinning on it
//!   still acknowledges the shootdown. When one returns, no processor can
//!   reach a removed frame or use a removed access through a stale
//!   translation, and a moved page is reachable only at its new address.
//! - A widening [`UserSpace::protect`] and [`UserSpace::resolve_fault`]
//!   only add access, so they send no shootdown. A slice elsewhere that
//!   still holds the narrower translation faults on it; the fault is
//!   spurious, and [`UserSpace::resolve_fault`] answers it as resolved, so
//!   the owner resumes the context and the processor walks the tables
//!   again.
//! - Each operation is atomic with respect to every other operation and
//!   to every slice. A caller that composes several (a query, then a
//!   mapping it decided on) serialises its own compositions; the contract
//!   orders nothing across calls.
//! - [`UserSpace::read`] and [`UserSpace::write`] copy through the kernel's
//!   own mapping of the backing frames, never through a user address. They
//!   may run while slices of the same space run elsewhere; user code sees
//!   the bytes as it would another thread's stores.
//! - A [`UserContext`] is single-owner: `Send`, never `Sync`, one slice at a
//!   time. [`UserContext::resume`] runs on the calling processor and returns
//!   with no processor-local state of the context left installed (address
//!   space root, thread pointer, system-call entry, extended state), so the
//!   owner may move the context to another processor between slices.
//! - A slice is bounded. It ends at the first interrupt the processor
//!   takes, its timer included, and a backend publishes this capability only
//!   when its timer interrupts every processor at a bounded period whatever
//!   user code does. A program that never traps therefore cannot hold a
//!   processor, and the context's owner observes every pending event within
//!   one timer period. An interrupt class the kernel itself keeps masked
//!   stays masked in user mode, so a slice never ends on an interrupt no
//!   handler will take.
//! - An asynchronous error the processor reports while user code runs (an
//!   AArch64 SError) ends the slice as a [`FaultKind::Bus`] fault of the
//!   program: the slice it interrupted is the one it is attributed to.
//! - `resume` is called from task context with interrupts enabled. It
//!   occupies its processor for at most one slice and does not suspend.
//! - A page may be mapped writable and executable at once, and user code
//!   makes its own stores visible to its instruction fetches by the means
//!   its ISA gives user mode: x86 keeps the two coherent, and an AArch64
//!   backend lets EL0 read `CTR_EL0` and run the `DC`/`IC` maintenance
//!   instructions on every processor before any slice runs there.
//! - Every register a slice can change, general, flags, vector and every
//!   extended-state component the processor enables for user mode, is saved
//!   when the slice ends and restored when the next one starts; the kernel's
//!   own state is saved and restored around the slice the same way.
//!
//! # Isolation contract
//!
//! A space keeps the kernel's half mapped while user code runs, so the
//! processor's privilege checks are all that separate the two, in both
//! directions. A backend publishes the capability only when every one of
//! its processors provides each of these, and otherwise refuses it with
//! [`NativeUnavailable::MissingProtection`] naming the first one missing:
//!
//! - Kernel mode cannot fetch an instruction from a page user mode can
//!   reach: x86 `CR4.SMEP` on every processor, AArch64 `PXN` on every
//!   EL0 mapping and on every table above one.
//! - Kernel mode cannot load from or store to such a page: x86 `CR4.SMAP`
//!   and AArch64 `PSTATE.PAN` (set on every exception entry by a clear
//!   `SCTLR_EL1.SPAN`) on every processor. The kernel never needs to lift
//!   either, because [`UserSpace::read`] and [`UserSpace::write`] reach
//!   user memory through the kernel's own mapping of its frames.
//! - User mode cannot read kernel memory through a speculative load the
//!   privilege check refuses only at retirement: the processor declares
//!   itself unaffected by rogue data cache load, x86
//!   `IA32_ARCH_CAPABILITIES.RDCL_NO` and AArch64 `ID_AA64PFR0_EL1.CSV3`.
//!   No processor that lacks it runs user code, because the kernel's half
//!   stays mapped and no page-table isolation separates it.

use core::convert::Infallible;

use thiserror::Error;

use crate::hv::RegisterId;
use crate::vmm::{PageFlags, VirtAddr, VirtRange};

/// The instruction set a native program is compiled for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionSet {
    X86_64,
    Aarch64,
    Riscv64,
}

/// A hardware protection the isolation contract requires, by its
/// architectural name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum Protection {
    /// x86 supervisor-mode execution prevention,
    /// `CPUID.(EAX=07H,ECX=0):EBX[7]`.
    #[error("SMEP (supervisor-mode execution prevention)")]
    Smep,
    /// x86 supervisor-mode access prevention,
    /// `CPUID.(EAX=07H,ECX=0):EBX[20]`.
    #[error("SMAP (supervisor-mode access prevention)")]
    Smap,
    /// x86 `IA32_ARCH_CAPABILITIES.RDCL_NO`, bit 0 of an MSR that
    /// `CPUID.(EAX=07H,ECX=0):EDX[29]` declares.
    #[error("RDCL_NO (not affected by rogue data cache load)")]
    RdclNo,
    /// AArch64 FEAT_PAN, `ID_AA64MMFR1_EL1.PAN` nonzero.
    #[error("FEAT_PAN (privileged access never)")]
    Pan,
    /// AArch64 FEAT_CSV3, `ID_AA64PFR0_EL1.CSV3` nonzero.
    #[error("FEAT_CSV3 (not affected by rogue data cache load)")]
    Csv3,
}

/// Why a platform offers no native user execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum NativeUnavailable {
    #[error("this platform has no native user execution")]
    Unsupported,
    #[error("native user execution requires {0}, which this processor lacks")]
    MissingProtection(Protection),
}

/// Why a native operation was refused. Every refusal leaves the space or
/// context it was asked of unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum NativeError {
    #[error("address range is empty, wraps, or lies outside the user range")]
    Address,
    #[error("range overlaps an existing user mapping")]
    Overlap,
    #[error("user memory is exhausted")]
    OutOfMemory,
    #[error("an address in the range is not mapped with the required access")]
    NotMapped,
    #[error("the register does not exist on this instruction set")]
    Register,
}

/// What a user slice ran into when it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The program executed the system-call instruction. Its arguments are
    /// in the context's registers, and the context resumes after it.
    SystemCall,
    /// The processor's timer interrupt ended the slice.
    Preempted,
    /// Any other interrupt ended the slice. The program did nothing.
    Interrupted,
    /// The program faulted. The context resumes at the faulting
    /// instruction, which faults again unless its cause was removed.
    Fault(UserFault),
}

/// An exception user code raised: synchronous, or the asynchronous error
/// of [`FaultKind::Bus`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserFault {
    pub kind: FaultKind,
    /// The faulting instruction.
    pub instruction: VirtAddr,
}

/// The class of a user fault, in the terms userspace ABIs deliver them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// An access the address space refused.
    Page {
        address: VirtAddr,
        /// The access attempted: exactly one of read, write or execute.
        access: PageFlags,
        /// Whether a translation existed and refused the access, rather
        /// than no translation existing at all.
        present: bool,
    },
    /// An undefined or unavailable instruction.
    InvalidInstruction,
    /// An integer division fault or an unmasked floating-point exception.
    Arithmetic,
    /// A breakpoint instruction or a debug trap.
    Breakpoint,
    /// A privileged or protection-violating instruction, a non-canonical
    /// address, or a control-flow-protection violation.
    Protection,
    /// A misaligned access the processor refused.
    Alignment,
    /// An external abort: the memory system reported an error for an access
    /// of the program, synchronously or as an asynchronous error taken while
    /// it ran. `instruction` is where the program resumes, not necessarily
    /// the access that caused the error.
    Bus,
}

/// The platform's native execution capability. [`crate::cpu::Cpu::native`]
/// exposes it only when every property the module docs state holds;
/// platforms without it use the uninhabited `Infallible` implementation.
pub trait NativeExecution: Send + Sync + 'static {
    type Space: UserSpace;
    type Context: UserContext<Space = Self::Space>;

    fn instruction_set(&self) -> InstructionSet;

    /// The addresses user code can map: the lowest page above the null
    /// guard up to the end of the user half.
    fn user_range(&self) -> VirtRange;

    /// An empty space: the kernel's half is present and inaccessible to
    /// user code, the user half has no mapping.
    fn create_space(&self) -> Result<Self::Space, NativeError>;

    /// A context whose first slice starts at `entry` with `stack` as its
    /// stack pointer, every other register in its architectural initial
    /// state.
    fn create_context(
        &self,
        entry: VirtAddr,
        stack: VirtAddr,
    ) -> Result<Self::Context, NativeError>;
}

/// A user address space. See the module docs for its concurrency contract.
///
/// # Safety
///
/// No user mapping aliases kernel memory or a frame another space owns, and
/// a page is reachable from user mode only with the flags it was mapped
/// with. Dropping a space releases its frames; the caller's borrow during
/// [`UserContext::resume`] guarantees no slice of it is running then.
pub unsafe trait UserSpace: Send + Sync + 'static {
    /// Installs zero-filled private pages covering `range` with `flags`,
    /// then copies `contents` to `range.start`. `range` need not be page
    /// aligned; every page it touches is installed. Fails, mapping nothing,
    /// when `contents` is longer than the range or any covered page is
    /// already mapped. Any combination of read, write and execute is a
    /// valid `flags`, write and execute together included.
    fn map(&self, range: VirtRange, contents: &[u8], flags: PageFlags) -> Result<(), NativeError>;

    /// Installs `byte_len` bytes (a page multiple) of zero-filled private
    /// pages at the first free range at or above `hint`, else at the first
    /// free range of the user range, and returns where.
    fn map_anywhere(
        &self,
        hint: VirtAddr,
        byte_len: usize,
        flags: PageFlags,
    ) -> Result<VirtRange, NativeError>;

    /// Copies `output.len()` bytes mapped readable starting at `address`.
    fn read(&self, address: VirtAddr, output: &mut [u8]) -> Result<(), NativeError>;

    /// Whether `byte_len` bytes from `address` are mapped writable.
    fn check_writable(&self, address: VirtAddr, byte_len: usize) -> Result<(), NativeError>;

    /// Copies `bytes` to `address`, which must be mapped writable over the
    /// whole length; nothing is written otherwise.
    fn write(&self, address: VirtAddr, bytes: &[u8]) -> Result<(), NativeError>;

    /// Atomically loads the 32-bit little-endian word at the 4-byte-aligned
    /// `address`, coherent with user-mode atomic operations on every
    /// processor. Fails with [`NativeError::Address`] when `address` is
    /// misaligned or outside the user range, and [`NativeError::NotMapped`]
    /// when the page is not mapped readable.
    fn atomic_load_u32(&self, address: VirtAddr) -> Result<u32, NativeError>;

    /// Atomically stores `value` at the 4-byte-aligned `address`, committing a
    /// discarded writable page first and coherent with user-mode atomic
    /// operations on every processor.
    fn atomic_store_u32(&self, address: VirtAddr, value: u32) -> Result<(), NativeError>;

    /// Atomically compares the 32-bit word at the 4-byte-aligned `address`
    /// with `expected` and, if equal, replaces it with `desired`, coherent
    /// with user-mode atomic operations on every processor. Requires both
    /// read and write access on the page.
    fn atomic_compare_exchange_u32(
        &self,
        address: VirtAddr,
        expected: u32,
        desired: u32,
    ) -> Result<Result<u32, u32>, NativeError>;

    /// Removes every mapped page of `range`, a page-aligned range inside
    /// the user range, and frees its frame once the shootdown completes.
    /// Pages of the range that are not mapped are skipped.
    fn unmap(&self, range: VirtRange) -> Result<(), NativeError>;

    /// Replaces the access of every page of the page-aligned `range` with
    /// `flags`, any combination of read, write and execute. Fails, changing
    /// nothing, when a page of the range is not mapped. Pages that lose an
    /// access are shot down before it returns.
    fn protect(&self, range: VirtRange, flags: PageFlags) -> Result<(), NativeError>;

    /// Releases the frame behind every mapped page of the page-aligned
    /// `range` once the shootdown completes, keeping the page mapped with
    /// its access: its next access reads zeros, and commits a zero-filled
    /// frame through [`Self::resolve_fault`] or a copy. Pages of the range
    /// that are not mapped are skipped.
    fn discard(&self, range: VirtRange) -> Result<(), NativeError>;

    /// Moves the pages of `from` to the start of `to`, each keeping its
    /// frame (or its discarded state) and its access, and installs
    /// zero-filled pages with the same access over the rest of `to`. Both
    /// ranges are page aligned and disjoint, `to` is at least as long as
    /// `from` and has no mapping, and every page of `from` is mapped with
    /// one access; anything else fails with nothing changed. When it
    /// returns, `from` has no mapping and no processor holds a translation
    /// of it.
    fn remap(&self, from: VirtRange, to: VirtRange) -> Result<(), NativeError>;

    /// Answers a page fault a slice reported at `address` for `access`
    /// (one of read, write or execute). Succeeds, and the context may
    /// resume, when the page is mapped with that access: a discarded page
    /// gets its zero-filled frame here, and a page that already has one
    /// faulted on a translation an earlier, narrower state left on that
    /// processor. Fails with [`NativeError::NotMapped`] when the page is
    /// not mapped with `access`, so the fault is the program's.
    fn resolve_fault(&self, address: VirtAddr, access: PageFlags) -> Result<(), NativeError>;

    /// The first run of the page-aligned `range` mapped contiguously with
    /// one access, clipped to `range`: the lowest mapped page of the range
    /// starts it, and the first unmapped page or change of access ends it.
    /// `None` when no page of the range is mapped.
    fn mapping(&self, range: VirtRange) -> Option<Mapping>;

    /// The first page-aligned free range of `byte_len` bytes, a nonzero
    /// page multiple, at or above `hint`, else in the whole user range:
    /// the placement [`Self::map_anywhere`] chooses. `None` when none is
    /// free.
    fn free_range(&self, hint: VirtAddr, byte_len: usize) -> Option<VirtAddr>;
}

/// A run of pages mapped with one access, as [`UserSpace::mapping`]
/// reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub range: VirtRange,
    pub flags: PageFlags,
}

/// One thread of user execution. See the module docs for its concurrency
/// contract.
///
/// # Safety
///
/// `resume` enters user mode only with `space`'s translations and leaves
/// none of the context's processor-local state installed when it returns.
/// No register write lets user code run with more privilege than user mode
/// or with a kernel address as its next instruction or stack.
pub unsafe trait UserContext: Send + 'static {
    type Space: UserSpace;

    /// A register of the detached snapshot.
    fn register(&self, register: RegisterId) -> Result<u64, NativeError>;

    /// Replaces a register of the detached snapshot. Flag bits user mode
    /// cannot hold are cleared when the next slice starts.
    fn set_register(&mut self, register: RegisterId, value: u64) -> Result<(), NativeError>;

    /// The thread pointer user code addresses its thread-local storage by.
    fn thread_pointer(&self) -> VirtAddr;

    /// Replaces the thread pointer; refused outside the user range.
    fn set_thread_pointer(&mut self, address: VirtAddr) -> Result<(), NativeError>;

    /// Duplicates the detached architectural snapshot (general, flags, thread
    /// pointer, vector and extended state) into an independent context for a
    /// new thread or child process.
    fn try_clone(&self) -> Result<Self, NativeError>
    where
        Self: Sized;

    /// Runs one slice against `space`.
    fn resume(&mut self, space: &Self::Space) -> Stop;
}

impl NativeExecution for Infallible {
    type Space = Self;
    type Context = Self;
    fn instruction_set(&self) -> InstructionSet {
        match *self {}
    }
    fn user_range(&self) -> VirtRange {
        match *self {}
    }
    fn create_space(&self) -> Result<Self, NativeError> {
        match *self {}
    }
    fn create_context(&self, _: VirtAddr, _: VirtAddr) -> Result<Self, NativeError> {
        match *self {}
    }
}

// SAFETY: no value of this type exists, so none of these methods can run.
unsafe impl UserSpace for Infallible {
    fn map(&self, _: VirtRange, _: &[u8], _: PageFlags) -> Result<(), NativeError> {
        match *self {}
    }
    fn map_anywhere(&self, _: VirtAddr, _: usize, _: PageFlags) -> Result<VirtRange, NativeError> {
        match *self {}
    }
    fn read(&self, _: VirtAddr, _: &mut [u8]) -> Result<(), NativeError> {
        match *self {}
    }
    fn check_writable(&self, _: VirtAddr, _: usize) -> Result<(), NativeError> {
        match *self {}
    }
    fn write(&self, _: VirtAddr, _: &[u8]) -> Result<(), NativeError> {
        match *self {}
    }
    fn atomic_load_u32(&self, _: VirtAddr) -> Result<u32, NativeError> {
        match *self {}
    }
    fn atomic_store_u32(&self, _: VirtAddr, _: u32) -> Result<(), NativeError> {
        match *self {}
    }
    fn atomic_compare_exchange_u32(
        &self,
        _: VirtAddr,
        _: u32,
        _: u32,
    ) -> Result<Result<u32, u32>, NativeError> {
        match *self {}
    }
    fn unmap(&self, _: VirtRange) -> Result<(), NativeError> {
        match *self {}
    }
    fn protect(&self, _: VirtRange, _: PageFlags) -> Result<(), NativeError> {
        match *self {}
    }
    fn discard(&self, _: VirtRange) -> Result<(), NativeError> {
        match *self {}
    }
    fn remap(&self, _: VirtRange, _: VirtRange) -> Result<(), NativeError> {
        match *self {}
    }
    fn resolve_fault(&self, _: VirtAddr, access: PageFlags) -> Result<(), NativeError> {
        let _ = access;
        match *self {}
    }
    fn mapping(&self, _: VirtRange) -> Option<Mapping> {
        match *self {}
    }
    fn free_range(&self, _: VirtAddr, _: usize) -> Option<VirtAddr> {
        match *self {}
    }
}

// SAFETY: uninhabited; there is no user execution to enter or leave.
unsafe impl UserContext for Infallible {
    type Space = Self;
    fn register(&self, _: RegisterId) -> Result<u64, NativeError> {
        match *self {}
    }
    fn set_register(&mut self, _: RegisterId, _: u64) -> Result<(), NativeError> {
        match *self {}
    }
    fn thread_pointer(&self) -> VirtAddr {
        match *self {}
    }
    fn set_thread_pointer(&mut self, _: VirtAddr) -> Result<(), NativeError> {
        match *self {}
    }
    fn try_clone(&self) -> Result<Self, NativeError> {
        match *self {}
    }
    fn resume(&mut self, _: &Self) -> Stop {
        match *self {}
    }
}
