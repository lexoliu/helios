use std::io;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, ShellError>;

#[derive(Debug, Error)]
pub enum ShellError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl ShellError {
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub fn unsupported(feature: &'static str) -> Self {
        Self::Message(format!("{feature} is not supported"))
    }
}
