#![no_std]
#![no_main]
use core::fmt::Write;
use core::result::Result::Ok;
use riscv_rt::entry;

pub struct SbiConsole;

impl Write for SbiConsole {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            let ret = sbi_rt::console_write_byte(byte);
            if ret.is_err() {
                return Err(core::fmt::Error);
            }
        }
        Ok(())
    }
}

#[unsafe(export_name = "_mp_hook")]
#[unsafe(link_section = ".text.mp_hook")]
pub extern "Rust" fn mp_hook(_hartid: usize) -> bool {
    true
}

#[entry]
fn main(hart_id: usize, opaque: usize, fdt_addr: usize) -> ! {
    let _ = (hart_id, opaque, fdt_addr);

    let console = SbiConsole;
    let heap_start = core::ptr::addr_of_mut!(__sheap);
    let heap_end = core::ptr::addr_of_mut!(__eheap);
    let heap_len = heap_end.addr() - heap_start.addr();
    let heap = unsafe { core::slice::from_raw_parts_mut(heap_start, heap_len) };

    helios_kernel::init(helios_kernel::Platform::new(console, heap));

    loop {}
}

unsafe extern "C" {
    static mut __sheap: u8;
    static mut __eheap: u8;
}
