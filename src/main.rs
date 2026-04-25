#![no_std]
#![cfg_attr(target_os = "none", no_main)]

extern crate alloc;

#[cfg(not(target_os = "none"))]
fn main() {
    helios_hosted::main();
}

// otherwise, the entry point is defined by the active bare-metal platform crate.

#[cfg(target_arch = "aarch64")]
use helios_aarch64 as _;
#[cfg(target_arch = "riscv64")]
use helios_riscv as _;
#[cfg(target_arch = "x86_64")]
use helios_x86 as _;
