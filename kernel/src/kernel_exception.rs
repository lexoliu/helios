#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelExceptionCause {
    InstructionFault,
    DataFault,
    IllegalInstruction,
    Breakpoint,
    Arithmetic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelException {
    pub cause: KernelExceptionCause,
    pub instruction_pointer: usize,
    pub frame_pointer: usize,
    pub faulting_address: Option<usize>,
}

pub type KernelNativeTrapHandler = extern "C" fn(
    instruction_pointer: usize,
    frame_pointer: usize,
    has_faulting_address: bool,
    faulting_address: usize,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelExceptionDispatch {
    Unhandled,
}

impl KernelException {
    pub fn dispatch_to(self, handler: KernelNativeTrapHandler) -> KernelExceptionDispatch {
        handler(
            self.instruction_pointer,
            self.frame_pointer,
            self.faulting_address.is_some(),
            self.faulting_address.unwrap_or(0),
        );
        KernelExceptionDispatch::Unhandled
    }
}
