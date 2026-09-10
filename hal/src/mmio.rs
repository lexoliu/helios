//! What one memory-mapped I/O access is, at the level of the instruction
//! the processor issues.
//!
//! Every accessor here compiles to exactly one single-register load or
//! store, with no writeback, no index register and no offset. That is a
//! stronger promise than a volatile access makes, and a driver running
//! under a hypervisor depends on it.
//!
//! # Why the addressing mode is part of the contract
//!
//! On AArch64 a stage-2 data abort taken on a guest access to an
//! unbacked address carries a decodable instruction syndrome
//! (`ESR_EL2.ISV = 1`) only when the faulting instruction is a
//! single-register load or store without writeback. A hypervisor
//! emulates the device access from that syndrome and from nothing else:
//! it never fetches or decodes the guest instruction. QEMU's HVF
//! accelerator aborts the process when the bit is clear
//! (`Assertion failed: (isv), function hvf_handle_exception`), and
//! Linux KVM on arm64 refuses the same access with "load/store
//! instruction decoding not implemented". Only a full emulator, which
//! decodes the instruction itself, tolerates it, which is why an
//! emulated lane can stay green while every real hypervisor dies.
//!
//! `read_volatile` and `write_volatile` do not give that promise. They
//! fix the width of an access, forbid eliding, merging and duplicating
//! it, and keep it ordered against other volatile accesses — but the
//! addressing mode is left to the code generator, which is free to fold
//! a loop's induction variable into a post-index form. A byte-at-a-time
//! device configuration read compiled as `ldrb w12, [x10], #0x1` is a
//! correct volatile access and an undecodable one.
//!
//! So on AArch64 the bodies below are `asm!` blocks with the addressing
//! form written out. On every other target they are the volatile
//! accesses, because neither the RISC-V nor the x86-64 encodings a
//! compiler emits for a volatile access carry the same hazard.
//!
//! This is the one module in `hal/` that selects on the instruction
//! set, and the selection is a definition rather than a choice: the
//! kernel never asks which processor it is on, but what an MMIO access
//! *is* is a property of the instruction set, exactly as Linux's
//! `readb`/`writeb` family is written per architecture.
//!
//! # Ordering
//!
//! Each accessor is one access and nothing more. None of them implies a
//! barrier: a caller ordering a register write against a descriptor it
//! published in memory, or against another processor, issues the fence
//! it already issues, and gets no help here.
//!
//! The `asm!` blocks are deliberately neither `nomem` nor `readonly`,
//! so the compiler may not move an ordinary memory access across one. A
//! read is not marked `readonly` because a device register read is not
//! generally free of effect — a read-to-clear status register is a write
//! as far as the device is concerned — and a driver that fills a
//! descriptor before reading the register that kicks a queue relies on
//! the store staying where it was written.

macro_rules! mmio_accessors {
    ($(
        $width:literal, $ty:ty, $read:ident, $write:ident, $load:literal, $store:literal, $reg:literal;
    )*) => {$(
        #[doc = concat!("Reads the ", $width, "-bit device register at `ptr`.")]
        ///
        /// One single-register, non-writeback load; see the module
        /// documentation for why the addressing mode is load-bearing.
        ///
        /// # Safety
        ///
        /// `ptr` must address a device register of this width that is
        /// mapped for the duration of the call and naturally aligned,
        /// and the read must be one the device accepts at this point in
        /// its protocol: this is a side-effecting access, not a load
        /// from memory.
        #[inline]
        pub unsafe fn $read(ptr: *const $ty) -> $ty {
            #[cfg(target_arch = "aarch64")]
            let value = {
                let value: $ty;
                unsafe {
                    core::arch::asm!(
                        concat!($load, " {value:", $reg, "}, [{ptr}]"),
                        ptr = in(reg) ptr,
                        value = out(reg) value,
                        options(nostack, preserves_flags),
                    );
                }
                value
            };
            #[cfg(not(target_arch = "aarch64"))]
            let value = unsafe { ptr.read_volatile() };

            value
        }

        #[doc = concat!("Writes `value` to the ", $width, "-bit device register at `ptr`.")]
        ///
        /// One single-register, non-writeback store; see the module
        /// documentation for why the addressing mode is load-bearing.
        ///
        /// # Safety
        ///
        /// `ptr` must address a device register of this width that is
        /// mapped for the duration of the call and naturally aligned,
        /// and the write must be one the device accepts at this point
        /// in its protocol.
        #[inline]
        pub unsafe fn $write(ptr: *mut $ty, value: $ty) {
            #[cfg(target_arch = "aarch64")]
            unsafe {
                core::arch::asm!(
                    concat!($store, " {value:", $reg, "}, [{ptr}]"),
                    ptr = in(reg) ptr,
                    value = in(reg) value,
                    options(nostack, preserves_flags),
                );
            }
            #[cfg(not(target_arch = "aarch64"))]
            unsafe {
                ptr.write_volatile(value);
            }
        }
    )*};
}

mmio_accessors! {
    "8", u8, read_u8, write_u8, "ldrb", "strb", "w";
    "16", u16, read_u16, write_u16, "ldrh", "strh", "w";
    "32", u32, read_u32, write_u32, "ldr", "str", "w";
    "64", u64, read_u64, write_u64, "ldr", "str", "x";
}
