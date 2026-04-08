use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum IoError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    AlreadyExists,
    #[error("not a directory")]
    NotDirectory,
    #[error("is a directory")]
    IsDirectory,
    #[error("directory is not empty")]
    DirectoryNotEmpty,
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
    #[error("device configuration is invalid: {0}")]
    InvalidDeviceConfig(&'static str),
    #[error("device fault")]
    DeviceFault,
}

pub type IoResult<T> = Result<T, IoError>;
