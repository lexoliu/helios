#![no_std]

pub const HELIOS_COMPILER_ABI_VERSION: u32 = 1;

pub const HELIOS_COMPILER_ALLOC: &str = "helios_compiler_alloc";
pub const HELIOS_COMPILER_FREE: &str = "helios_compiler_free";
pub const HELIOS_COMPILER_COMPILE: &str = "helios_compiler_compile";
pub const HELIOS_COMPILER_INITIALIZE: &str = "helios_compiler_initialize";
pub const HELIOS_COMPILER_PTHREAD_SELF_OFFSET: &str = "helios_compiler_pthread_self_offset";

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompileHint {
    Fast = 0,
    Balanced = 1,
    Performance = 2,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompilerStatus {
    Ok = 0,
    InvalidRequest = 1,
    InvalidInput = 2,
    CompileFailed = 3,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    CoreModule = 0,
    Component = 1,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompilerRequestHeader {
    pub abi_version: u32,
    pub hint: CompileHint,
    pub wasm_ptr: u32,
    pub wasm_len: u32,
    pub target_ptr: u32,
    pub target_len: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompilerResponseHeader {
    pub abi_version: u32,
    pub status: CompilerStatus,
    pub output_kind: OutputKind,
    pub precompiled_ptr: u32,
    pub precompiled_len: u32,
    pub diagnostic_ptr: u32,
    pub diagnostic_len: u32,
}
