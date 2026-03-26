#![no_std]

use core::fmt::Write;

pub struct Platform<Console: Write + Send + 'static> {
    pub console: Console,
    pub heap: &'static mut [u8],
}

impl<Console: Write + Send + 'static> Platform<Console> {
    pub const fn new(console: Console, heap: &'static mut [u8]) -> Self {
        Self { console, heap }
    }
}
