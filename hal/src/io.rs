#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoError {
    InvalidBufferLength {
        required_multiple: usize,
        actual: usize,
    },
    PermissionDenied,
    OutOfBounds,
    ReadOnly,
    Unsupported,
    DeviceFault,
}

pub type IoResult<T> = Result<T, IoError>;
