use std::io;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, ShellError>;

#[derive(Debug, Error)]
pub enum ShellError {
    #[error("{0}")]
    Message(String),
    #[error("{0}")]
    ShellDiagnostic(String),
    #[error("{0}: is read only")]
    ReadonlyVariable(String),
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

    pub fn shell_diagnostic(message: impl Into<String>) -> Self {
        Self::ShellDiagnostic(message.into())
    }

    pub fn readonly_variable(name: impl Into<String>) -> Self {
        Self::ReadonlyVariable(name.into())
    }
}
