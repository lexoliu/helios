#![no_std]
#![cfg_attr(target_os = "none", no_main)]

extern crate alloc;

#[cfg(not(target_os = "none"))]
fn main() {
    helios_hosted::main();
}

// otherwise, the entry point is defined in the hal and riscv crates, so we don't need to define it here

#[cfg(target_arch = "riscv64")]
use helios_riscv as _;
