//! The x86-64 implementation of `helios_hal::native`.
//!
//! Every slice entry and exit writes CR3 without PCID and no user mapping is
//! global, so no processor keeps a usable user translation once its slice
//! ends: a page removed from a space while none of its slices runs needs no
//! remote invalidation, and one removed while some run goes through the
//! kernel's TLB shootdown broadcast (`smp::shootdown_tlb_range`). The
//! kernel's upper half is shared with every space; a space owns only its
//! private lower half.

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use helios_hal::hv::RegisterId;
use helios_hal::native::{
    FaultKind, InstructionSet, NativeError, NativeExecution, Protection, Stop, UserContext,
    UserFault, UserSpace,
};
use helios_hal::vmm::{PageFlags, VirtAddr, VirtRange};
use helios_kernel::{NativeSpace, UserTranslation};
use x86::msr::{rdmsr, wrmsr};
use x86_64::registers::control::{Cr2, Cr3};
use x86_64::structures::paging::mapper::MapToError;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr as X86VirtAddr};

use crate::isolation::UserIsolation;
use crate::{exceptions, extended_state, smp};

// GDT: null, kernel code/data, user data/code. RPL is three for user selectors.
pub(crate) const USER_DATA: u16 = 3 * 8 + 3;
pub(crate) const USER_CODE: u16 = 4 * 8 + 3;
const PAGE_BYTES: u64 = 4096;
// The end of the lower canonical half with four-level paging.
const USER_END: u64 = 1 << 47;
// AMD64 architectural MSR numbers, also specified by Intel SDM volume 4.
const EFER: u32 = 0xc000_0080;
const STAR: u32 = 0xc000_0081;
const LSTAR: u32 = 0xc000_0082;
const FMASK: u32 = 0xc000_0084;
const FS_BASE: u32 = 0xc000_0100;
const GS_BASE: u32 = 0xc000_0101;
const KERNEL_GS_BASE: u32 = 0xc000_0102;
// A software vector number above the IDT's 256, so the exit path can tell a
// SYSCALL from every hardware vector.
const SYSCALL_VECTOR: u64 = 256;
// Page-fault error code bits (Intel SDM vol. 3A, 4.7): P, W/R, U/S, I/D.
const PAGE_FAULT_PRESENT: u64 = 1 << 0;
const PAGE_FAULT_WRITE: u64 = 1 << 1;
const PAGE_FAULT_USER: u64 = 1 << 2;
const PAGE_FAULT_FETCH: u64 = 1 << 4;
// The `ret` opcode (Intel SDM vol. 2B, RET: C3, near return).
const RET: u8 = 0xc3;

/// The x86-64 native execution capability.
pub struct X86Native;

impl NativeExecution for X86Native {
    type Space = X86Space;
    type Context = X86Context;

    fn instruction_set(&self) -> InstructionSet {
        InstructionSet::X86_64
    }

    fn user_range(&self) -> VirtRange {
        VirtRange::new(
            VirtAddr::new(PAGE_BYTES as usize),
            (USER_END - PAGE_BYTES) as usize,
        )
    }

    fn create_space(&self) -> Result<X86Space, NativeError> {
        let mut tables = X86Tables {
            root: Box::new(PageTable::new()),
            tables: Tables {
                frames: Vec::new(),
                offset: smp::current_runtime().physical_memory_offset,
            },
        };
        tables.sync_kernel_half();
        Ok(NativeSpace::new(tables, PAGE_BYTES, USER_END))
    }

    fn create_context(&self, entry: VirtAddr, stack: VirtAddr) -> Result<X86Context, NativeError> {
        X86Context::new(entry.raw() as u64, stack.raw() as u64)
    }
}

struct Tables {
    frames: Vec<Box<PageTable>>,
    offset: usize,
}

// SAFETY: each allocation is a fresh aligned, zeroed table retained until the
// space is dropped. No frame is allocated twice.
unsafe impl FrameAllocator<Size4KiB> for Tables {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let table = Box::new(PageTable::new());
        let physical = (&*table as *const PageTable as usize).checked_sub(self.offset)?;
        let frame =
            PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(physical as u64)).ok()?;
        self.frames.push(table);
        Some(frame)
    }
}

/// A process's lower-half page tables: a private root whose upper half is
/// the kernel's, and the tables below it, freed with the space.
pub struct X86Tables {
    root: Box<PageTable>,
    tables: Tables,
}

// SAFETY: owned by one space and written only under its lock.
unsafe impl Send for X86Tables {}

/// A user address space.
pub type X86Space = NativeSpace<X86Tables>;

fn page_table_flags(flags: PageFlags) -> PageTableFlags {
    let mut table = PageTableFlags::PRESENT;
    if !flags.is_empty() {
        table |= PageTableFlags::USER_ACCESSIBLE;
    }
    if flags.contains(PageFlags::WRITE) {
        table |= PageTableFlags::WRITABLE;
    }
    if !flags.contains(PageFlags::EXECUTE) {
        table |= PageTableFlags::NO_EXECUTE;
    }
    table
}

impl X86Tables {
    fn mapper(&mut self) -> OffsetPageTable<'_> {
        // SAFETY: the root and every lower-half table are owned by this
        // space, and the kernel direct map reaches each of them.
        unsafe { OffsetPageTable::new(&mut self.root, X86VirtAddr::new(self.tables.offset as u64)) }
    }
}

// SAFETY: a user page maps exactly the given frame with the given access;
// the upper half is copied from the kernel root and never user-accessible.
unsafe impl UserTranslation for X86Tables {
    fn install(&mut self, base: u64, physical: u64, flags: PageFlags) -> Result<(), NativeError> {
        let frame = PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(physical))
            .expect("the user allocator returns page-aligned frames");
        let page = Page::<Size4KiB>::from_start_address(X86VirtAddr::new(base))
            .expect("a page span is page aligned");
        let offset = X86VirtAddr::new(self.tables.offset as u64);
        // Every table above a leaf grants user, write and execute access and
        // the leaf alone limits it, as Linux's `_PAGE_TABLE` does: a page
        // first mapped with no access must still reach user mode once
        // `protect` widens its leaf.
        let parents =
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        // SAFETY: the page is not mapped and the frame is fresh; the root and
        // every lower-half table are owned by this space and reached through
        // the kernel direct map.
        let mapped = unsafe {
            OffsetPageTable::new(&mut self.root, offset).map_to_with_table_flags(
                page,
                frame,
                page_table_flags(flags),
                parents,
                &mut self.tables,
            )
        };
        match mapped {
            // The page was not present, so no processor can cache it.
            Ok(flush) => {
                flush.ignore();
                Ok(())
            }
            Err(MapToError::FrameAllocationFailed) => Err(NativeError::OutOfMemory),
            Err(error) => panic!("mapping unmapped user page {base:#x}: {error:?}"),
        }
    }

    fn uninstall(&mut self, base: u64) {
        let page = Page::<Size4KiB>::from_start_address(X86VirtAddr::new(base))
            .expect("an installed page is page aligned");
        match self.mapper().unmap(page) {
            // The space's caller invalidates the page wherever a slice may
            // hold it (`invalidate`).
            Ok((_, flush)) => flush.ignore(),
            Err(error) => panic!("removing user page {base:#x}: {error:?}"),
        }
    }

    fn protect(&mut self, base: u64, flags: PageFlags) {
        let page = Page::<Size4KiB>::from_start_address(X86VirtAddr::new(base))
            .expect("an installed page is page aligned");
        // SAFETY: the page maps a frame of this space; only its access
        // changes, and the caller invalidates a narrowing.
        match unsafe { self.mapper().update_flags(page, page_table_flags(flags)) } {
            Ok(flush) => flush.ignore(),
            Err(error) => panic!("changing the access of user page {base:#x}: {error:?}"),
        }
    }

    fn invalidate(&self, first: u64, last: u64) {
        let (start, byte_len) = (first as usize, (last - first) as usize);
        // The kernel's CR3 is loaded here, so the local INVLPG reaches no
        // user translation: the slice exit's CR3 write already dropped
        // them. A processor running a slice takes the broadcast's IPI from
        // user mode, and its exit's CR3 write drops the space's
        // translations before the handler acknowledges.
        smp::flush_tlb_range_local(start, byte_len);
        smp::shootdown_tlb_range(start, byte_len);
    }

    fn physical(&self, frame: NonNull<u8>) -> u64 {
        (frame.as_ptr() as usize - self.tables.offset) as u64
    }
}

impl X86Tables {
    /// Copies any newly populated upper-half PML4 entries from the current
    /// kernel `CR3` with single 64-bit volatile writes so concurrent slices on
    /// other processors never observe torn or cleared upper-half descriptors.
    fn sync_kernel_half(&mut self) {
        let kernel = Cr3::read().0.start_address().as_u64() as usize + self.tables.offset;
        // SAFETY: the current CR3 is a live kernel root, mapped through the
        // direct map; both tables hold 512 64-bit entries.
        let kernel_words = unsafe { &*(kernel as *const [u64; 512]) };
        let root_words = core::ptr::from_mut(&mut *self.root).cast::<u64>();
        for (index, &entry) in kernel_words.iter().enumerate().skip(256) {
            // SAFETY: `index < 512` and `self.root` is uniquely borrowed under
            // the space lock.
            unsafe {
                let slot = root_words.add(index);
                if slot.read_volatile() != entry {
                    slot.write_volatile(entry);
                }
            }
        }
    }

    /// The physical root for a slice, with the kernel's current upper half.
    fn root(&mut self) -> u64 {
        self.sync_kernel_half();
        &*self.root as *const PageTable as u64 - self.tables.offset as u64
    }
}

/// Hardware register order shared with the exception entry stubs.
#[derive(Default, Debug)]
#[repr(C)]
struct Registers {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rdi: u64,
    rsi: u64,
    rbp: u64,
    rbx: u64,
    rdx: u64,
    rcx: u64,
    rax: u64,
    vector: u64,
    error: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

impl Registers {
    /// The word of the frame that holds `register`.
    fn word(register: RegisterId) -> Result<usize, NativeError> {
        use core::mem::offset_of;
        let offset = match register {
            RegisterId::X86Gpr(0) => offset_of!(Self, rax),
            RegisterId::X86Gpr(1) => offset_of!(Self, rcx),
            RegisterId::X86Gpr(2) => offset_of!(Self, rdx),
            RegisterId::X86Gpr(3) => offset_of!(Self, rbx),
            RegisterId::X86Gpr(4) => offset_of!(Self, rsp),
            RegisterId::X86Gpr(5) => offset_of!(Self, rbp),
            RegisterId::X86Gpr(6) => offset_of!(Self, rsi),
            RegisterId::X86Gpr(7) => offset_of!(Self, rdi),
            RegisterId::X86Gpr(8) => offset_of!(Self, r8),
            RegisterId::X86Gpr(9) => offset_of!(Self, r9),
            RegisterId::X86Gpr(10) => offset_of!(Self, r10),
            RegisterId::X86Gpr(11) => offset_of!(Self, r11),
            RegisterId::X86Gpr(12) => offset_of!(Self, r12),
            RegisterId::X86Gpr(13) => offset_of!(Self, r13),
            RegisterId::X86Gpr(14) => offset_of!(Self, r14),
            RegisterId::X86Gpr(15) => offset_of!(Self, r15),
            RegisterId::X86Rip => offset_of!(Self, rip),
            RegisterId::X86Rflags => offset_of!(Self, rflags),
            _ => return Err(NativeError::Register),
        };
        Ok(offset / size_of::<u64>())
    }

    fn get(&self, register: RegisterId) -> Result<u64, NativeError> {
        let word = Self::word(register)?;
        // SAFETY: the frame is `repr(C)` and made of `u64` words only, and
        // `word` names one of them.
        Ok(unsafe { (self as *const Self as *const u64).add(word).read() })
    }

    fn set(&mut self, register: RegisterId, value: u64) -> Result<(), NativeError> {
        let word = Self::word(register)?;
        // SAFETY: as in `get`.
        unsafe { (self as *mut Self as *mut u64).add(word).write(value) };
        Ok(())
    }
}

struct AlignedBytes {
    pointer: NonNull<u8>,
    layout: Layout,
}
impl AlignedBytes {
    fn new(size: usize, align: usize) -> Result<Self, NativeError> {
        let layout = Layout::from_size_align(size, align).expect("native allocation layout");
        // SAFETY: layout is nonempty and valid; Drop uses the same layout.
        let pointer =
            NonNull::new(unsafe { alloc_zeroed(layout) }).ok_or(NativeError::OutOfMemory)?;
        Ok(Self { pointer, layout })
    }
}
impl Drop for AlignedBytes {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe {
            dealloc(self.pointer.as_ptr(), self.layout);
        }
    }
}
// SAFETY: uniquely owned storage, with no active assembly use across a move.
unsafe impl Send for AlignedBytes {}

#[repr(C)]
struct Switch {
    registers: Registers,
    user_xstate: u64,
    kernel_xstate: u64,
    kernel_rsp: u64,
    kernel_cr3: u64,
    kernel_fs: u64,
    user_fs: u64,
    kernel_gs: u64,
    entry_stack: u64,
    scratch_rsp: u64,
    user_cr3: u64,
    self_pointer: u64,
}

/// One user thread: its register file, its XSAVE area, and the kernel
/// stack its system calls and traps enter on.
pub struct X86Context {
    switch: Switch,
    _user_xstate: AlignedBytes,
    _kernel_xstate: AlignedBytes,
    _entry_stack: AlignedBytes,
}

impl X86Context {
    fn new(entry: u64, stack: u64) -> Result<Self, NativeError> {
        if !(PAGE_BYTES..USER_END).contains(&entry) || !(PAGE_BYTES..USER_END).contains(&stack) {
            return Err(NativeError::Address);
        }
        let size = extended_state::xsave_sizes().0 as usize;
        let user = AlignedBytes::new(size, 64)?;
        let kernel = AlignedBytes::new(size, 64)?;
        let entry_stack = AlignedBytes::new(exceptions::EXCEPTION_STACK_BYTES, 16)?;
        // The all-zero XSAVE header restores architectural initial state.
        Ok(Self {
            switch: Switch {
                registers: Registers {
                    rip: entry,
                    rsp: stack,
                    cs: USER_CODE as u64,
                    ss: USER_DATA as u64,
                    rflags: 0x202,
                    ..Registers::default()
                },
                user_xstate: user.pointer.as_ptr() as u64,
                kernel_xstate: kernel.pointer.as_ptr() as u64,
                kernel_rsp: 0,
                kernel_cr3: 0,
                kernel_fs: 0,
                user_fs: 0,
                kernel_gs: 0,
                entry_stack: entry_stack.pointer.as_ptr() as u64 + entry_stack.layout.size() as u64,
                scratch_rsp: 0,
                user_cr3: 0,
                self_pointer: 0,
            },
            _user_xstate: user,
            _kernel_xstate: kernel,
            _entry_stack: entry_stack,
        })
    }

    fn fault(&self, vector: u64, address: u64) -> UserFault {
        let error = self.switch.registers.error;
        let kind = match vector {
            0 | 16 | 19 => FaultKind::Arithmetic,
            1 | 3 => FaultKind::Breakpoint,
            6 | 7 => FaultKind::InvalidInstruction,
            4 | 5 | 10 | 11 | 12 | 13 | 21 => FaultKind::Protection,
            14 => FaultKind::Page {
                address: VirtAddr::new(address as usize),
                access: if error & PAGE_FAULT_FETCH != 0 {
                    PageFlags::EXECUTE
                } else if error & PAGE_FAULT_WRITE != 0 {
                    PageFlags::WRITE
                } else {
                    PageFlags::READ
                },
                present: error & PAGE_FAULT_PRESENT != 0,
            },
            17 => FaultKind::Alignment,
            vector => panic!("x86 user mode raised exception vector {vector}, which has no stub"),
        };
        UserFault {
            kind,
            instruction: VirtAddr::new(self.switch.registers.rip as usize),
        }
    }
}

// SAFETY: `resume` loads `space`'s root only for the slice and restores the
// kernel's CR3, FS, GS and extended state before returning; the selectors,
// RFLAGS and canonical RIP/RSP checks keep user code in ring three.
unsafe impl UserContext for X86Context {
    type Space = X86Space;

    fn register(&self, register: RegisterId) -> Result<u64, NativeError> {
        self.switch.registers.get(register)
    }

    fn set_register(&mut self, register: RegisterId, value: u64) -> Result<(), NativeError> {
        self.switch.registers.set(register, value)
    }

    fn thread_pointer(&self) -> VirtAddr {
        VirtAddr::new(self.switch.user_fs as usize)
    }

    fn set_thread_pointer(&mut self, address: VirtAddr) -> Result<(), NativeError> {
        if address.raw() as u64 >= USER_END {
            return Err(NativeError::Address);
        }
        self.switch.user_fs = address.raw() as u64;
        Ok(())
    }

    fn try_clone(&self) -> Result<Self, NativeError> {
        let size = self._user_xstate.layout.size();
        let user = AlignedBytes::new(size, 64)?;
        let kernel = AlignedBytes::new(size, 64)?;
        let entry_stack = AlignedBytes::new(exceptions::EXCEPTION_STACK_BYTES, 16)?;
        // SAFETY: both XSAVE buffers are uniquely owned and `size` bytes long.
        unsafe {
            core::ptr::copy_nonoverlapping(
                self._user_xstate.pointer.as_ptr(),
                user.pointer.as_ptr(),
                size,
            );
        }
        Ok(Self {
            switch: Switch {
                registers: Registers {
                    ..self.switch.registers
                },
                user_xstate: user.pointer.as_ptr() as u64,
                kernel_xstate: kernel.pointer.as_ptr() as u64,
                kernel_rsp: 0,
                kernel_cr3: 0,
                kernel_fs: 0,
                user_fs: self.switch.user_fs,
                kernel_gs: 0,
                entry_stack: entry_stack.pointer.as_ptr() as u64 + entry_stack.layout.size() as u64,
                scratch_rsp: 0,
                user_cr3: 0,
                self_pointer: 0,
            },
            _user_xstate: user,
            _kernel_xstate: kernel,
            _entry_stack: entry_stack,
        })
    }

    fn resume(&mut self, space: &X86Space) -> Stop {
        // SYSCALL can save a noncanonical next RIP or stack. Refuse it before
        // IRET, whose fault would otherwise occur at kernel privilege.
        if self.switch.registers.rip >= USER_END || self.switch.registers.rsp >= USER_END {
            return Stop::Fault(UserFault {
                kind: FaultKind::Protection,
                instruction: VirtAddr::new(self.switch.registers.rip as usize),
            });
        }
        // Preserve user arithmetic/debug flags, RF, AC and ID. NT, VM, IOPL,
        // VIF/VIP and reserved flags cannot be carried into the kernel's IRET.
        const USER_FLAGS: u64 = (1 << 0)
            | (1 << 2)
            | (1 << 4)
            | (1 << 6)
            | (1 << 7)
            | (1 << 8)
            | (1 << 10)
            | (1 << 11)
            | (1 << 16)
            | (1 << 18)
            | (1 << 21);
        self.switch.registers.rflags = (self.switch.registers.rflags & USER_FLAGS) | 0x202;
        // Counted until the exit below has written the kernel's CR3 back.
        let slice = space.begin_slice();
        self.switch.user_cr3 = slice.with_tables(X86Tables::root);
        let fault_address = x86_64::instructions::interrupts::without_interrupts(|| unsafe {
            self.switch.self_pointer = core::ptr::from_mut(&mut self.switch) as u64;
            let (kernel_frame, kernel_flags) = Cr3::read_raw();
            self.switch.kernel_cr3 =
                kernel_frame.start_address().as_u64() | u64::from(kernel_flags);
            self.switch.kernel_fs = rdmsr(FS_BASE);
            self.switch.kernel_gs = rdmsr(GS_BASE);
            let old_kernel_gs = rdmsr(KERNEL_GS_BASE);
            wrmsr(KERNEL_GS_BASE, self.switch.self_pointer);
            wrmsr(GS_BASE, 0);
            // SYSCALL loads kernel CS=8 and SS=16; return uses IRET, not SYSRET.
            wrmsr(STAR, 8 << 32);
            wrmsr(LSTAR, helios_x86_native_syscall as *const () as u64);
            wrmsr(FMASK, u32::MAX as u64);
            wrmsr(EFER, rdmsr(EFER) | 1);
            helios_x86_native_enter(&mut self.switch);
            wrmsr(GS_BASE, self.switch.kernel_gs);
            wrmsr(KERNEL_GS_BASE, old_kernel_gs);
            // Interrupts stay masked from the user fault to here, so CR2 is
            // still the user fault's address.
            Cr2::read_raw()
        });
        drop(slice);
        match self.switch.registers.vector {
            SYSCALL_VECTOR => Stop::SystemCall,
            vector if vector == exceptions::TIMER_INTERRUPT_VECTOR as u64 => Stop::Preempted,
            32..=255 => Stop::Interrupted,
            vector => Stop::Fault(self.fault(vector, fault_address)),
        }
    }
}

global_asm!(include_str!("native.S"),
    user_data = const USER_DATA,
    user_code = const USER_CODE,
    syscall_vector = const SYSCALL_VECTOR,
    user_xstate = const core::mem::offset_of!(Switch, user_xstate),
    kernel_xstate = const core::mem::offset_of!(Switch, kernel_xstate),
    kernel_rsp = const core::mem::offset_of!(Switch, kernel_rsp),
    kernel_cr3 = const core::mem::offset_of!(Switch, kernel_cr3),
    kernel_fs = const core::mem::offset_of!(Switch, kernel_fs),
    user_fs = const core::mem::offset_of!(Switch, user_fs),
    entry_stack = const core::mem::offset_of!(Switch, entry_stack),
    scratch_rsp = const core::mem::offset_of!(Switch, scratch_rsp),
    user_cr3 = const core::mem::offset_of!(Switch, user_cr3),
    self_pointer = const core::mem::offset_of!(Switch, self_pointer),
    frame_words = const core::mem::size_of::<Registers>() / 8,
);

unsafe extern "C" {
    fn helios_x86_native_enter(state: *mut Switch);
    fn helios_x86_native_syscall();
    fn helios_x86_isolation_load();
    fn helios_x86_isolation_store();
}

/// Marks a recorded error code; no page-fault error code bit reaches 63
/// (Intel SDM vol. 3A, 4.7; AMD APM vol. 2, 8.4.2).
const RECORDED: u64 = 1 << 63;

/// One processor's record of [`verify_user_isolation`]: the first of the
/// two user pages under test while the check runs, and the page-fault
/// error code of each ring-0 access the processor refused there. Written
/// by its processor only, from the check and from the page-fault
/// dispatcher it interrupts.
pub(crate) struct IsolationProbe {
    code: AtomicU64,
    fetch: AtomicU64,
    load: AtomicU64,
    store: AtomicU64,
}

impl IsolationProbe {
    pub(crate) const fn new() -> Self {
        Self {
            code: AtomicU64::new(0),
            fetch: AtomicU64::new(0),
            load: AtomicU64::new(0),
            store: AtomicU64::new(0),
        }
    }

    /// Records a ring-0 page fault at `address` with error code `error`
    /// when it is on the pages under test, and says whether it was.
    pub(crate) fn claim(&self, address: usize, error: u64) -> bool {
        let code = self.code.load(Ordering::Relaxed);
        if code == 0 || !(code..code + 2 * PAGE_BYTES).contains(&(address as u64)) {
            return false;
        }
        let slot = if error & PAGE_FAULT_FETCH != 0 {
            &self.fetch
        } else if error & PAGE_FAULT_WRITE != 0 {
            &self.store
        } else {
            &self.load
        };
        slot.store(error | RECORDED, Ordering::Relaxed);
        true
    }
}

/// Proves, on the calling processor, that ring 0 can neither fetch from
/// nor load from nor store to a user page: the SMEP and SMAP half of the
/// isolation contract, checked against a page this module mapped.
///
/// The check maps a user code page holding a `ret` and a user data page in
/// a fresh space, loads the space's root as a slice entry does, and from
/// ring 0 calls the code page, then calls the load and store helpers of
/// `native.S` on the data page. Each access must raise a supervisor
/// protection fault, which the page-fault dispatcher records through
/// [`IsolationProbe::claim`] and answers by resuming the call's return.
/// An access that completes instead (the `ret` comes straight back) is a
/// boot failure naming the protection that is not in effect.
///
/// A processor without the whole contract runs no user code, so there is
/// nothing to check on it.
pub(crate) fn verify_user_isolation(isolation: UserIsolation) {
    let runtime = smp::current_runtime();
    if let Err(reason) = isolation.require() {
        tracing::warn!(
            target: "helios_x86::native",
            processor = runtime.logical_id(),
            %reason,
            "native user execution is unavailable"
        );
        return;
    }
    let code = X86Native.user_range().start;
    let data = VirtAddr::new(code.raw() + PAGE_BYTES as usize);
    let space = X86Native
        .create_space()
        .unwrap_or_else(|error| panic!("x86 isolation check could not create a space: {error}"));
    for (page, contents, flags) in [
        (code, &[RET][..], PageFlags::READ | PageFlags::EXECUTE),
        (data, &[][..], PageFlags::READ | PageFlags::WRITE),
    ] {
        space
            .map(VirtRange::new(page, PAGE_BYTES as usize), contents, flags)
            .unwrap_or_else(|error| {
                panic!(
                    "x86 isolation check could not map user page {:#x}: {error}",
                    page.raw()
                )
            });
    }
    let probe = &runtime.isolation_probe;
    let slice = space.begin_slice();
    let user_cr3 = slice.with_tables(X86Tables::root);
    probe.code.store(code.raw() as u64, Ordering::Relaxed);
    x86_64::instructions::interrupts::without_interrupts(|| {
        // SAFETY: the user root shares the kernel's upper half, which
        // holds everything this code, its stack and the dispatcher touch.
        // The user pages are this space's own, and every access to one
        // either faults, and resumes at its call's return, or runs the
        // `ret` and the helpers, which change nothing but RAX, RDI and
        // the data page. The kernel's CR3 goes back before interrupts
        // do, and its write drops every translation of the space.
        unsafe {
            asm!(
                "mov {kernel}, cr3",
                "mov cr3, {user}",
                "call rdi",
                "mov rdi, rsi",
                "call {load}",
                "call {store}",
                "mov cr3, {kernel}",
                user = in(reg) user_cr3,
                kernel = out(reg) _,
                load = sym helios_x86_isolation_load,
                store = sym helios_x86_isolation_store,
                inout("rdi") code.raw() => _,
                in("rsi") data.raw(),
                out("rax") _,
            );
        }
    });
    probe.code.store(0, Ordering::Relaxed);
    drop(slice);
    drop(space);
    let accesses = [
        (
            "fetch from",
            &probe.fetch,
            code,
            PAGE_FAULT_FETCH,
            Protection::Smep,
        ),
        ("load from", &probe.load, data, 0, Protection::Smap),
        (
            "store to",
            &probe.store,
            data,
            PAGE_FAULT_WRITE,
            Protection::Smap,
        ),
    ];
    for (access, slot, page, kind, protection) in accesses {
        let error = slot.swap(0, Ordering::Relaxed);
        assert!(
            error & RECORDED != 0,
            "x86 processor {} completed a ring-0 {access} user page {:#x}: {protection} is \
             not in effect",
            runtime.logical_id(),
            page.raw()
        );
        let expected = PAGE_FAULT_PRESENT | kind;
        assert!(
            error & (PAGE_FAULT_PRESENT | PAGE_FAULT_USER | PAGE_FAULT_WRITE | PAGE_FAULT_FETCH)
                == expected,
            "x86 processor {}'s ring-0 {access} user page {:#x} faulted with error code \
             {:#x}, not as the supervisor protection fault {protection} raises",
            runtime.logical_id(),
            page.raw(),
            error & !RECORDED
        );
    }
    tracing::info!(
        target: "helios_x86::native",
        processor = runtime.logical_id(),
        code = code.raw(),
        data = data.raw(),
        "ring-0 fetch from, load from and store to user pages fault"
    );
}

#[unsafe(no_mangle)]
extern "C" fn helios_x86_native_interrupt(frame: &mut exceptions::ExceptionFrame) {
    exceptions::helios_x86_interrupt_dispatch(frame);
}
