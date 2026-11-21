//! Go's two failure paths: returned errors, which callers wrap with context, and panics,
//! which `Generate` and `Validate` recover with one prefix and no intermediate context.

use std::fmt::Display;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Failed(String),
    /// A Go `panic` value; context wrappers pass it through unchanged.
    #[error("{0}")]
    Panic(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    pub fn panic(message: impl Display) -> Self {
        Self::Panic(message.to_string())
    }

    /// Go's `fmt.Errorf("<context>: %w", err)`; a panic unwinds past the wrapper.
    #[must_use]
    pub fn wrap(self, context: impl Display) -> Self {
        match self {
            Self::Failed(message) => Self::Failed(format!("{context}: {message}")),
            panic @ Self::Panic(_) => panic,
        }
    }

    /// Go's `recover()` at an exported entry point: the panic becomes an ordinary error.
    #[must_use]
    pub fn recover(self, context: impl Display) -> Self {
        match self {
            Self::Panic(message) => Self::Failed(format!("{context}: {message}")),
            failed @ Self::Failed(_) => failed,
        }
    }
}

pub trait Wrap<T> {
    fn wrap(self, context: impl Display) -> Result<T>;
}

impl<T> Wrap<T> for Result<T> {
    fn wrap(self, context: impl Display) -> Result<T> {
        self.map_err(|error| error.wrap(context))
    }
}

/// Converts a library error into a Go panic value.
pub trait OrPanic<T> {
    fn or_panic(self) -> Result<T>;
}

impl<T, E: Display> OrPanic<T> for std::result::Result<T, E> {
    fn or_panic(self) -> Result<T> {
        self.map_err(Error::panic)
    }
}
