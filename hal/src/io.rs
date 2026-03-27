use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum IoError {
    #[error("buffer length {actual} is invalid; it must be a multiple of {required_multiple}")]
    InvalidBufferLength {
        required_multiple: usize,
        actual: usize,
    },
    #[error("permission denied")]
    PermissionDenied,
    #[error("requested range is out of bounds")]
    OutOfBounds,
    #[error("resource is read-only")]
    ReadOnly,
    #[error("operation is not supported")]
    Unsupported,
    #[error("device fault")]
    DeviceFault,
}

pub type IoResult<T> = Result<T, IoError>;
