//! The AArch64 implementation of `helios_hal::native`: user code at EL0
//! translated through a per-process TTBR0.
//!
//! Every slice entry and exit invalidates this processor's non-global
//! translations of ASID 0, which every user page is, so no processor keeps a
//! usable user translation once its slice ends; the kernel's own TTBR1
//! mappings are global and survive. A page removed or narrowed while a slice
//! of its space runs is invalidated with the inner-shareable broadcast TLBI
//! the kernel's own address space uses. The kernel's TTBR0 and TCR are
//! restored at every exit.

use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use core::mem::offset_of;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use arm_sysregs::el1::registers::SctlrEl1;
use helios_hal::hv::RegisterId;
use helios_hal::native::{
    FaultKind, InstructionSet, NativeError, NativeExecution, Stop, UserContext, UserFault,
    UserSpace,
};
use helios_hal::vmm::{PageFlags, VirtAddr, VirtRange};
use helios_kernel::{
    NativeSpace, UserTranslation, allocate_user_frame_zeroed_on, deallocate_user_frame_on,
};

use crate::isolation::UserIsolation;

const PAGE: u64 = 4096;
/// The end of a 48-bit TTBR0 range (`TCR_EL1.T0SZ` = 16).
const USER_END: u64 = 1 << 48;
const ENTRIES: usize = 512;

// VMSAv8-64 stage 1 descriptor bits (Arm ARM D8.3).
const TABLE: u64 = 0b11;
const PAGE_DESCRIPTOR: u64 = 0b11;
const AP_EL0_RW: u64 = 0b01 << 6;
const AP_EL0_RO: u64 = 0b11 << 6;
const SH_INNER: u64 = 0b11 << 8;
const AF: u64 = 1 << 10;
const NOT_GLOBAL: u64 = 1 << 11;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
// Table descriptor PXNTable: nothing below the table is executable at
// EL1, as Linux's `PUD_TABLE_PXN` and `PMD_TABLE_PXN` make every table
// of a user address space.
const PXN_TABLE: u64 = 1 << 59;
const ADDRESS: u64 = 0x0000_ffff_ffff_f000;

// SPSR_EL1: the NZCV flags user code keeps; M = 0 is EL0t (AArch64, so
// M[4] = 0 and no AArch32 state is ever entered).
const SPSR_NZCV: u64 = 0xf << 28;
// SPSR_EL1.F: FIQs stay masked in user mode as in the kernel, which takes
// none (`entry.S` clears only PSTATE.I); a FIQ unmasked at EL0 would end
// every slice with nothing to acknowledge it. IRQ, SError and debug
// exceptions stay unmasked.
const SPSR_FIQ_MASKED: u64 = 1 << 6;

// `ESR_EL1.EC` classes taken from EL0 (Arm ARM D19.2 "ESR_EL1").
const EC_SVC64: u64 = 0x15;
const EC_INSTRUCTION_ABORT_LOWER: u64 = 0x20;
const EC_PC_ALIGNMENT: u64 = 0x22;
const EC_DATA_ABORT_LOWER: u64 = 0x24;
const EC_SP_ALIGNMENT: u64 = 0x26;
const EC_FP_EXCEPTION: u64 = 0x2c;
const EC_BREAKPOINT_LOWER: u64 = 0x30;
const EC_STEP_LOWER: u64 = 0x32;
const EC_WATCHPOINT_LOWER: u64 = 0x34;
const EC_BRK64: u64 = 0x3c;
const EC_ILLEGAL_STATE: u64 = 0x0e;
// Data/instruction fault status codes, bits [5:2]: translation, access
// flag, permission, and a synchronous external abort on a table walk at
// levels 0 to 3. Whole codes: 0b100001 is an alignment fault, 0b010000 a
// synchronous external abort on the access itself.
const FSC_TRANSLATION: u64 = 0b0001;
const FSC_ACCESS_FLAG: u64 = 0b0010;
const FSC_PERMISSION: u64 = 0b0011;
const FSC_EXTERNAL_WALK: u64 = 0b0101;
const FSC_ALIGNMENT: u64 = 0b10_0001;
const FSC_EXTERNAL: u64 = 0b01_0000;
const ISS_WNR: u64 = 1 << 6;

const EXIT_SYNC: u64 = 1;
const EXIT_IRQ: u64 = 2;
const EXIT_FIQ: u64 = 3;
const EXIT_SERROR: u64 = 4;

/// Lets user code synchronise its own instruction stream with the data it
/// wrote, as Linux's `INIT_SCTLR_EL1_MMU_ON` lets it:
/// - `SCTLR_EL1.UCT` makes `CTR_EL0` readable at EL0;
/// - `SCTLR_EL1.UCI` lets EL0 run `DC CVAU`, `DC CVAC`, `DC CIVAC` and
///   `IC IVAU`.
///
/// Code a program writes into a writable executable page runs only after
/// those, which `__clear_cache` issues. Neither bit changes what EL1 may do.
/// Each processor sets them once, before it runs any user slice.
pub(crate) fn enable_user_cache_maintenance() {
    let bits = (SctlrEl1::UCT | SctlrEl1::UCI).bits();
    // SAFETY: sets two SCTLR_EL1 bits that govern only EL0 accesses; the
    // `isb` makes them take effect before any later user slice.
    unsafe {
        asm!(
            "mrs {sctlr}, sctlr_el1",
            "orr {sctlr}, {sctlr}, {bits}",
            "msr sctlr_el1, {sctlr}",
            "isb",
            sctlr = out(reg) _,
            bits = in(reg) bits,
            options(nostack, preserves_flags),
        );
    }
}

/// The AArch64 native execution capability.
pub struct Aarch64Native;

impl NativeExecution for Aarch64Native {
    type Space = NativeSpace<Aarch64Tables>;
    type Context = Aarch64Context;

    fn instruction_set(&self) -> InstructionSet {
        InstructionSet::Aarch64
    }

    fn user_range(&self) -> VirtRange {
        VirtRange::new(VirtAddr::new(PAGE as usize), (USER_END - PAGE) as usize)
    }

    fn create_space(&self) -> Result<Self::Space, NativeError> {
        Ok(NativeSpace::new(Aarch64Tables::new()?, PAGE, USER_END))
    }

    fn create_context(
        &self,
        entry: VirtAddr,
        stack: VirtAddr,
    ) -> Result<Aarch64Context, NativeError> {
        let (entry, stack) = (entry.raw() as u64, stack.raw() as u64);
        if !(PAGE..USER_END).contains(&entry) || !(PAGE..USER_END).contains(&stack) {
            return Err(NativeError::Address);
        }
        Ok(Aarch64Context {
            switch: Switch {
                pc: entry,
                sp: stack,
                ..Switch::default()
            },
        })
    }
}

/// A process's TTBR0 tables, every level a user-memory frame.
pub struct Aarch64Tables {
    root: NonNull<u8>,
    /// Every table below the root, freed with the space.
    tables: Vec<NonNull<u8>>,
    offset: usize,
}

// SAFETY: the tables are owned by the space and written only under its lock.
unsafe impl Send for Aarch64Tables {}

fn zeroed_table() -> Result<NonNull<u8>, NativeError> {
    allocate_user_frame_zeroed_on(helios_hal::cpu::current_processor())
        .map_err(|_| NativeError::OutOfMemory)
}

impl Aarch64Tables {
    fn new() -> Result<Self, NativeError> {
        Ok(Self {
            root: zeroed_table()?,
            tables: Vec::new(),
            offset: crate::physical_memory_offset(),
        })
    }

    fn entries(&self, table: u64) -> *mut u64 {
        (table as usize + self.offset) as *mut u64
    }

    /// The last-level entry of `base`, creating the tables above it.
    fn leaf(&mut self, base: u64) -> Result<*mut u64, NativeError> {
        let mut table = self.root.as_ptr() as u64 - self.offset as u64;
        for level in 0..3 {
            let index = (base >> (39 - 9 * level)) as usize & (ENTRIES - 1);
            // SAFETY: `table` is a table of this space, reached through the
            // direct map, and `index` is below its 512 entries.
            let entry = unsafe { self.entries(table).add(index) };
            // SAFETY: as above; the space's lock excludes every other writer.
            let descriptor = unsafe { entry.read_volatile() };
            table = if descriptor & TABLE == TABLE {
                descriptor & ADDRESS
            } else {
                let next = zeroed_table()?;
                self.tables.push(next);
                let physical = next.as_ptr() as u64 - self.offset as u64;
                // SAFETY: as above. The zeroed table is visible to the walker
                // before the descriptor that names it (`dsb` in `publish`).
                unsafe {
                    asm!("dsb ishst", options(nostack, preserves_flags));
                    entry.write_volatile(physical | TABLE | PXN_TABLE);
                }
                physical
            };
        }
        let index = (base >> 12) as usize & (ENTRIES - 1);
        // SAFETY: the last-level table of `base`, as above.
        Ok(unsafe { self.entries(table).add(index) })
    }
}

/// The last-level descriptor of a user page mapping `physical` with EL0
/// access `flags`.
fn page_descriptor(physical: u64, flags: PageFlags) -> u64 {
    let access = if flags.contains(PageFlags::WRITE) {
        AP_EL0_RW
    } else if flags.is_empty() {
        // No EL0 access at all; EL1 never uses a user address.
        0
    } else {
        AP_EL0_RO
    };
    let execute = if flags.contains(PageFlags::EXECUTE) {
        0
    } else {
        UXN
    };
    // MAIR index 0 is normal write-back memory (`crate::vmm`).
    physical | PAGE_DESCRIPTOR | access | SH_INNER | AF | NOT_GLOBAL | PXN | execute
}

fn publish() {
    // Descriptor writes reach every processor's table walker before any
    // slice that may use them starts.
    // SAFETY: a barrier only.
    unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
}

// SAFETY: a user page maps exactly the given frame, with EL0 access no
// wider than `flags`, never executable at EL1, and only below 2^48 in
// TTBR0, which holds no kernel mapping.
unsafe impl UserTranslation for Aarch64Tables {
    fn install(&mut self, base: u64, physical: u64, flags: PageFlags) -> Result<(), NativeError> {
        let leaf = self.leaf(base)?;
        // SAFETY: the leaf entry of a page that is not mapped.
        unsafe { leaf.write_volatile(page_descriptor(physical, flags)) };
        publish();
        Ok(())
    }

    fn uninstall(&mut self, base: u64) {
        let leaf = self.leaf(base).expect("an installed page has its tables");
        // SAFETY: as in `install`. The space's caller invalidates the page
        // wherever a slice may hold it (`invalidate`).
        unsafe { leaf.write_volatile(0) };
        publish();
    }

    fn protect(&mut self, base: u64, flags: PageFlags) {
        let leaf = self.leaf(base).expect("an installed page has its tables");
        // SAFETY: as in `install`. Only the access bits change, which the
        // Arm ARM's break-before-make rules do not require a break for; the
        // caller invalidates a narrowing.
        unsafe {
            let physical = leaf.read_volatile() & ADDRESS;
            leaf.write_volatile(page_descriptor(physical, flags));
        }
        publish();
    }

    fn invalidate(&self, first: u64, last: u64) {
        // SAFETY: TLB maintenance and barriers only. `vale1is` broadcasts
        // to every processor in the inner-shareable domain, the `dsb ish`
        // after it waits until each has completed it, and the operand is
        // VA[55:12] with ASID 0, which every user page is tagged with.
        unsafe {
            asm!("dsb ishst", options(nostack, preserves_flags));
            for base in (first..last).step_by(PAGE as usize) {
                asm!("tlbi vale1is, {}", in(reg) base >> 12, options(nostack, preserves_flags));
            }
            asm!("dsb ish", "isb", options(nostack, preserves_flags));
        }
    }

    fn physical(&self, frame: NonNull<u8>) -> u64 {
        frame.as_ptr() as u64 - self.offset as u64
    }
}

impl Aarch64Tables {
    fn root_physical(&self) -> u64 {
        self.root.as_ptr() as u64 - self.offset as u64
    }
}

impl Drop for Aarch64Tables {
    fn drop(&mut self) {
        let processor = helios_hal::cpu::current_processor();
        for table in self.tables.drain(..) {
            deallocate_user_frame_on(processor, table);
        }
        deallocate_user_frame_on(processor, self.root);
    }
}

/// A slice's register record, shared with `native.S`.
#[derive(Default)]
#[repr(C)]
#[derive(Clone)]
struct Switch {
    x: [u64; 31],
    sp: u64,
    pc: u64,
    pstate: u64,
    tpidr: u64,
    fpsr: u64,
    fpcr: u64,
    exit: u64,
    esr: u64,
    far: u64,
    kernel_sp: u64,
    kernel_sp_el0: u64,
    kernel_ttbr0: u64,
    kernel_tcr: u64,
    kernel_tpidr: u64,
    kernel_fpcr: u64,
    user_ttbr0: u64,
    user_tcr: u64,
    q: [u128; 32],
}

/// One user thread: its general, vector and system registers.
pub struct Aarch64Context {
    switch: Switch,
}

impl Aarch64Context {
    fn word(&mut self, register: RegisterId) -> Result<&mut u64, NativeError> {
        match register {
            RegisterId::Aarch64X(index) if index < 31 => Ok(&mut self.switch.x[index as usize]),
            RegisterId::Aarch64Sp => Ok(&mut self.switch.sp),
            RegisterId::Aarch64Pc => Ok(&mut self.switch.pc),
            RegisterId::Aarch64Pstate => Ok(&mut self.switch.pstate),
            _ => Err(NativeError::Register),
        }
    }

    fn fault(&self) -> UserFault {
        let esr = self.switch.esr;
        let class = esr >> 26;
        let status = esr & 0x3f;
        let page = |access| match status >> 2 {
            _ if status == FSC_ALIGNMENT => FaultKind::Alignment,
            // Synchronous external aborts: Linux's `do_sea` class.
            _ if status == FSC_EXTERNAL => FaultKind::Bus,
            FSC_EXTERNAL_WALK => FaultKind::Bus,
            FSC_TRANSLATION => FaultKind::Page {
                address: VirtAddr::new(self.switch.far as usize),
                access,
                present: false,
            },
            FSC_ACCESS_FLAG | FSC_PERMISSION => FaultKind::Page {
                address: VirtAddr::new(self.switch.far as usize),
                access,
                present: true,
            },
            // Address-size aborts and the rest address nothing a mapping
            // could provide.
            _ => FaultKind::Protection,
        };
        let kind = match class {
            EC_DATA_ABORT_LOWER => page(if esr & ISS_WNR != 0 {
                PageFlags::WRITE
            } else {
                PageFlags::READ
            }),
            EC_INSTRUCTION_ABORT_LOWER => page(PageFlags::EXECUTE),
            EC_PC_ALIGNMENT | EC_SP_ALIGNMENT => FaultKind::Alignment,
            EC_FP_EXCEPTION => FaultKind::Arithmetic,
            EC_BREAKPOINT_LOWER | EC_STEP_LOWER | EC_WATCHPOINT_LOWER | EC_BRK64 => {
                FaultKind::Breakpoint
            }
            EC_ILLEGAL_STATE => FaultKind::Protection,
            // Unknown instructions and trapped system-register, SVE or
            // WFx accesses: Linux's `do_el0_undef` class.
            _ => FaultKind::InvalidInstruction,
        };
        UserFault {
            kind,
            instruction: VirtAddr::new(self.switch.pc as usize),
        }
    }
}

// SAFETY: `resume` enters EL0t with the space's TTBR0 and restores the
// kernel's TTBR0, TCR, SP_EL0, TPIDR_EL0, FPCR and callee-saved registers
// before it returns; SPSR is rebuilt from NZCV and the kernel's FIQ mask
// alone, so user code never leaves AArch64 EL0 or masks an IRQ or SError.
unsafe impl UserContext for Aarch64Context {
    type Space = NativeSpace<Aarch64Tables>;

    fn register(&self, register: RegisterId) -> Result<u64, NativeError> {
        match register {
            RegisterId::Aarch64X(index) if index < 31 => Ok(self.switch.x[index as usize]),
            RegisterId::Aarch64Sp => Ok(self.switch.sp),
            RegisterId::Aarch64Pc => Ok(self.switch.pc),
            RegisterId::Aarch64Pstate => Ok(self.switch.pstate),
            _ => Err(NativeError::Register),
        }
    }

    fn set_register(&mut self, register: RegisterId, value: u64) -> Result<(), NativeError> {
        *self.word(register)? = value;
        Ok(())
    }

    fn thread_pointer(&self) -> VirtAddr {
        VirtAddr::new(self.switch.tpidr as usize)
    }

    fn set_thread_pointer(&mut self, address: VirtAddr) -> Result<(), NativeError> {
        if address.raw() as u64 >= USER_END {
            return Err(NativeError::Address);
        }
        self.switch.tpidr = address.raw() as u64;
        Ok(())
    }

    fn try_clone(&self) -> Result<Self, NativeError> {
        Ok(Self {
            switch: self.switch.clone(),
        })
    }

    fn resume(&mut self, space: &NativeSpace<Aarch64Tables>) -> Stop {
        self.switch.pstate = (self.switch.pstate & SPSR_NZCV) | SPSR_FIQ_MASKED;
        // Counted until the exit below has dropped this processor's
        // translations of ASID 0. ASID 0 in TTBR0[63:48]; every user page is
        // non-global under it.
        let slice = space.begin_slice();
        self.switch.user_ttbr0 = slice.with_tables(|tables| tables.root_physical());
        let tcr: u64;
        let daif: u64;
        // SAFETY: reads a system register and masks IRQs on this processor.
        unsafe {
            asm!("mrs {}, tcr_el1", out(reg) tcr, options(nomem, nostack, preserves_flags));
            asm!("mrs {}, daif", "msr daifset, #2", out(reg) daif, options(nostack));
        }
        self.switch.user_tcr =
            (tcr & !crate::lifecycle::IDMAP_TCR_TTBR0_MASK) | crate::lifecycle::IDMAP_TCR_TTBR0;
        // SAFETY: IRQs are masked from here to the exit, which restores
        // every piece of kernel state the entry replaced.
        unsafe { helios_aarch64_native_enter(&mut self.switch) };
        drop(slice);
        let stop = match self.switch.exit {
            EXIT_IRQ => {
                if crate::handle_pending_interrupts() {
                    Stop::Preempted
                } else {
                    Stop::Interrupted
                }
            }
            EXIT_SYNC if self.switch.esr >> 26 == EC_SVC64 => Stop::SystemCall,
            EXIT_SYNC => Stop::Fault(self.fault()),
            EXIT_FIQ => Stop::Interrupted,
            EXIT_SERROR => Stop::Fault(UserFault {
                kind: FaultKind::Bus,
                instruction: VirtAddr::new(self.switch.pc as usize),
            }),
            exit => panic!("native slice ended with unknown exit {exit}"),
        };
        // SAFETY: restores the mask read above.
        unsafe { asm!("msr daif, {}", in(reg) daif, options(nostack)) };
        stop
    }
}

global_asm!(include_str!("native.S"),
    sp = const offset_of!(Switch, sp),
    pc = const offset_of!(Switch, pc),
    pstate = const offset_of!(Switch, pstate),
    tpidr = const offset_of!(Switch, tpidr),
    fpsr = const offset_of!(Switch, fpsr),
    exit = const offset_of!(Switch, exit),
    esr = const offset_of!(Switch, esr),
    kernel_sp = const offset_of!(Switch, kernel_sp),
    kernel_sp_el0 = const offset_of!(Switch, kernel_sp_el0),
    kernel_ttbr0 = const offset_of!(Switch, kernel_ttbr0),
    kernel_tcr = const offset_of!(Switch, kernel_tcr),
    kernel_tpidr = const offset_of!(Switch, kernel_tpidr),
    kernel_fpcr = const offset_of!(Switch, kernel_fpcr),
    user_ttbr0 = const offset_of!(Switch, user_ttbr0),
    user_tcr = const offset_of!(Switch, user_tcr),
    q = const offset_of!(Switch, q),
    exit_sync = const EXIT_SYNC,
    exit_irq = const EXIT_IRQ,
    exit_fiq = const EXIT_FIQ,
    exit_serror = const EXIT_SERROR,
);

// The paired loads and stores of `native.S` reach these with a signed
// 7-bit scaled immediate.
const _: () = {
    assert!(offset_of!(Switch, fpcr) == offset_of!(Switch, fpsr) + 8);
    assert!(offset_of!(Switch, far) == offset_of!(Switch, esr) + 8);
    assert!(offset_of!(Switch, esr) <= 504 && offset_of!(Switch, fpsr) <= 504);
    assert!(offset_of!(Switch, q).is_multiple_of(16));
};

unsafe extern "C" {
    fn helios_aarch64_native_enter(switch: *mut Switch);
    fn helios_aarch64_isolation_load();
    fn helios_aarch64_isolation_store();
}

// `ESR_EL1.EC` classes taken without a change of exception level.
const EC_INSTRUCTION_ABORT_SAME: u64 = 0x21;
const EC_DATA_ABORT_SAME: u64 = 0x25;
// `RET` (Arm ARM C6.2, RET with x30), little-endian.
const RET: [u8; 4] = 0xd65f_03c0_u32.to_le_bytes();
/// Marks a recorded syndrome; `ESR_EL1` bit 63 is RES0.
const RECORDED: u64 = 1 << 63;

/// One processor's record of [`verify_user_isolation`]: the first of the
/// two user pages under test while the check runs, and the syndrome of
/// each EL1 access the processor refused there. Written by its processor
/// only, from the check and from the synchronous exception handler it
/// raises.
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

    /// Records an EL1 abort with syndrome `esr` at `far` when it is on the
    /// pages under test, and says whether it was.
    pub(crate) fn claim(&self, esr: usize, far: usize) -> bool {
        let code = self.code.load(Ordering::Relaxed);
        if code == 0 || !(code..code + 2 * PAGE).contains(&(far as u64)) {
            return false;
        }
        let esr = esr as u64;
        let slot = match esr >> 26 {
            EC_INSTRUCTION_ABORT_SAME => &self.fetch,
            EC_DATA_ABORT_SAME if esr & ISS_WNR != 0 => &self.store,
            EC_DATA_ABORT_SAME => &self.load,
            _ => return false,
        };
        slot.store(esr | RECORDED, Ordering::Relaxed);
        true
    }
}

/// Proves, on the calling processor, that EL1 can neither fetch from nor
/// load from nor store to a user page: the PXN and PAN half of the
/// isolation contract, checked against a page this module mapped.
///
/// The check maps a user code page and a user data page in a fresh space,
/// loads the space's TTBR0 and TCR as a slice entry does, and from EL1
/// branches with link to the code page, then to the load and store
/// helpers of `native.S` on the data page. Each access must raise a
/// permission fault, which the synchronous exception handler records
/// through [`IsolationProbe::claim`] and answers by resuming at the
/// branch's return address. An access that completes instead is a boot
/// failure naming the protection that is not in effect.
///
/// A processor without the whole contract runs no user code, so there is
/// nothing to check on it.
pub(crate) fn verify_user_isolation(isolation: UserIsolation) {
    let processor = helios_hal::cpu::current_processor();
    if let Err(reason) = isolation.require() {
        tracing::warn!(
            target: "helios_aarch64::native",
            processor = processor.id(),
            %reason,
            "native user execution is unavailable"
        );
        return;
    }
    // An executable page that EL0 cannot write, so that PXN alone refuses
    // the fetch (EL1 never executes an EL0-writable page whatever its
    // PXN), and a writable page above it for the load and the store.
    let code = Aarch64Native.user_range().start;
    let data = VirtAddr::new(code.raw() + PAGE as usize);
    let space = Aarch64Native.create_space().unwrap_or_else(|error| {
        panic!("AArch64 isolation check could not create a space: {error}")
    });
    for (page, contents, flags) in [
        (code, &RET[..], PageFlags::READ | PageFlags::EXECUTE),
        (data, &[][..], PageFlags::READ | PageFlags::WRITE),
    ] {
        space
            .map(VirtRange::new(page, PAGE as usize), contents, flags)
            .unwrap_or_else(|error| {
                panic!(
                    "AArch64 isolation check could not map user page {:#x}: {error}",
                    page.raw()
                )
            });
    }
    let probe = &crate::current_processor_runtime().isolation_probe;
    let slice = space.begin_slice();
    let user_ttbr0 = slice.with_tables(|tables| tables.root_physical());
    let tcr: u64;
    // SAFETY: reads a system register.
    unsafe { asm!("mrs {}, tcr_el1", out(reg) tcr, options(nomem, nostack, preserves_flags)) };
    let user_tcr =
        (tcr & !crate::lifecycle::IDMAP_TCR_TTBR0_MASK) | crate::lifecycle::IDMAP_TCR_TTBR0;
    probe.code.store(code.raw() as u64, Ordering::Relaxed);
    let daif = crate::read_daif();
    crate::mask_irq();
    // SAFETY: IRQs are masked from here to the restore below. TTBR0 holds
    // no kernel mapping this code, its stack or the exception handler
    // reach, so swapping it for the space's changes nothing they use, and
    // the TLBI after each swap leaves no translation of the other behind.
    // Every access to a user page either faults, and resumes at its
    // branch's return address, or runs the `ret` and the helpers, which
    // change nothing but x0, x1 and the data page.
    unsafe {
        asm!(
            "mrs {kernel_ttbr0}, ttbr0_el1",
            "msr tcr_el1, {user_tcr}",
            "msr ttbr0_el1, {user_ttbr0}",
            "isb",
            "tlbi aside1, xzr",
            "dsb nsh",
            "isb",
            "blr x0",
            "mov x0, x2",
            "bl {load}",
            "bl {store}",
            "msr tcr_el1, {kernel_tcr}",
            "msr ttbr0_el1, {kernel_ttbr0}",
            "isb",
            "tlbi aside1, xzr",
            "dsb nsh",
            "isb",
            kernel_ttbr0 = out(reg) _,
            user_tcr = in(reg) user_tcr,
            user_ttbr0 = in(reg) user_ttbr0,
            kernel_tcr = in(reg) tcr,
            load = sym helios_aarch64_isolation_load,
            store = sym helios_aarch64_isolation_store,
            inout("x0") code.raw() => _,
            out("x1") _,
            in("x2") data.raw(),
            out("x30") _,
        );
        crate::write_daif(daif);
    }
    probe.code.store(0, Ordering::Relaxed);
    drop(slice);
    drop(space);
    let accesses = [
        ("fetch from", &probe.fetch, code, "PXN"),
        ("load from", &probe.load, data, "PSTATE.PAN"),
        ("store to", &probe.store, data, "PSTATE.PAN"),
    ];
    for (access, slot, page, protection) in accesses {
        let syndrome = slot.swap(0, Ordering::Relaxed);
        assert!(
            syndrome & RECORDED != 0,
            "AArch64 processor {} completed an EL1 {access} user page {:#x}: {protection} is \
             not in effect",
            processor.id(),
            page.raw()
        );
        let syndrome = syndrome & !RECORDED;
        assert!(
            (syndrome & 0x3f) >> 2 == FSC_PERMISSION,
            "AArch64 processor {}'s EL1 {access} user page {:#x} faulted with syndrome \
             {syndrome:#x}, not as the permission fault {protection} raises",
            processor.id(),
            page.raw()
        );
    }
    tracing::info!(
        target: "helios_aarch64::native",
        processor = processor.id(),
        code = code.raw(),
        data = data.raw(),
        "EL1 fetch from, load from and store to user pages fault"
    );
}
