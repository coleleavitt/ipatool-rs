//! The decryptor's error type. The messages reproduce `core.py` and `decrypt.py`'s
//! `ValueError` texts.

use std::path::PathBuf;

/// Result alias for the decryptor.
pub type Result<T> = std::result::Result<T, Error>;

/// A decryption failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A malformed IPA, Mach-O, SINF or SUPF record, with the ported `core.py`/`decrypt.py`
    /// message.
    #[error("{0}")]
    Format(String),
    /// A filesystem failure, with the operation that caused it.
    #[error("{context}: {source}")]
    Io {
        /// What was attempted.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The output path already exists (`decrypt.py` refuses to overwrite).
    #[error("output already exists; never overwrite it: {0}")]
    OutputExists(PathBuf),
}

/// A [`Error::Format`] from any displayable message.
pub(crate) fn format_err(message: impl Into<String>) -> Error {
    Error::Format(message.into())
}

impl Error {
    /// A [`Error::Io`] from a context label and an I/O error.
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
