use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    /// Same text as Go's `keyring.ErrKeyNotFound`.
    #[error("The specified item could not be found in the keyring")]
    NotFound,
    #[error("{0}")]
    Passphrase(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid keyring item: {0}")]
    Item(#[from] serde_json::Error),
    #[error("invalid encrypted keyring file: {0}")]
    Token(&'static str),
    /// Wrong passphrase or a corrupted file.
    #[error("failed to decrypt keyring file: wrong passphrase or corrupted file")]
    Decrypt,
    #[error("failed to generate random bytes: {0}")]
    Random(String),
    #[cfg(all(unix, not(target_os = "macos")))]
    #[error(transparent)]
    SecretService(#[from] ::secret_service::Error),
    #[cfg(target_os = "macos")]
    #[error(transparent)]
    MacOs(#[from] security_framework::base::Error),
}
