//! Credential storage compatible with the Go ipatool, which uses
//! `github.com/byteness/keyring`. The same service name and keys give access to the
//! same entries from either tool:
//!
//! - macOS: a generic password per key in the login Keychain (service = service name,
//!   account = key);
//! - other Unix: a Secret Service collection named after the service, one item per key
//!   with attribute `profile=<key>`; the secret is the JSON of the Go `keyring.Item`;
//! - fallback: one JWE file per key in the state directory, encrypted with a passphrase
//!   (`PBES2-HS256+A128KW` + `A256GCM`, as `jose2go` writes it).
//!
//! As in Go, the backend is picked once when the keychain is opened: the first one
//! that is available wins.

mod error;
mod file;
mod item;
mod jwe;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(all(unix, not(target_os = "macos")))]
mod secret_service;

use std::path::{Path, PathBuf};

pub use error::{Error, Result};

/// Asks for the file-backend passphrase. Receives the directory being unlocked.
pub type PassphrasePrompt = Box<dyn Fn(&Path) -> Result<String, String> + Send + Sync>;

pub struct Config {
    /// Service name; Go uses `ipatool-auth.service`. Also stored as each item's label.
    pub service_name: String,
    /// Directory of the encrypted-file fallback (the state directory).
    pub file_dir: PathBuf,
    pub passphrase: PassphrasePrompt,
}

pub struct Keychain {
    backend: Backend,
    label: String,
}

enum Backend {
    #[cfg(target_os = "macos")]
    MacOs(macos::MacKeychain),
    #[cfg(all(unix, not(target_os = "macos")))]
    SecretService(secret_service::SecretServiceKeyring),
    File(file::FileKeyring),
}

impl Keychain {
    /// Opens the first available backend, in the Go order: macOS Keychain, Secret
    /// Service, encrypted files.
    pub async fn open(config: Config) -> Self {
        let label = config.service_name.clone();
        #[cfg(target_os = "macos")]
        {
            let _ = &config.file_dir;
            return Self {
                backend: Backend::MacOs(macos::MacKeychain::new(config.service_name)),
                label,
            };
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        if let Some(ring) = secret_service::SecretServiceKeyring::open(&config.service_name).await {
            return Self {
                backend: Backend::SecretService(ring),
                label,
            };
        }
        #[allow(unreachable_code, reason = "macOS always returns above")]
        Self {
            backend: Backend::File(file::FileKeyring::new(config.file_dir, config.passphrase)),
            label,
        }
    }

    /// Opens only the encrypted-file backend.
    #[must_use]
    pub fn open_file(config: Config) -> Self {
        Self {
            label: config.service_name,
            backend: Backend::File(file::FileKeyring::new(config.file_dir, config.passphrase)),
        }
    }

    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            #[cfg(target_os = "macos")]
            Backend::MacOs(_) => "keychain",
            #[cfg(all(unix, not(target_os = "macos")))]
            Backend::SecretService(_) => "secret-service",
            Backend::File(_) => "file",
        }
    }

    /// # Errors
    /// [`Error::NotFound`] when the key is not stored, or a backend error.
    pub async fn get(&self, key: &str) -> Result<Vec<u8>> {
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::MacOs(ring) => ring.get(key),
            #[cfg(all(unix, not(target_os = "macos")))]
            Backend::SecretService(ring) => ring.get(key).await,
            Backend::File(ring) => ring.get(key),
        }
    }

    /// # Errors
    /// A backend error.
    pub async fn set(&self, key: &str, data: &[u8]) -> Result<()> {
        let item = item::StoredItem::new(key, data, &self.label);
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::MacOs(ring) => ring.set(&item),
            #[cfg(all(unix, not(target_os = "macos")))]
            Backend::SecretService(ring) => ring.set(&item).await,
            Backend::File(ring) => ring.set(&item),
        }
    }

    /// Like Go, removing a missing key is [`Error::NotFound`] except on Secret
    /// Service, where it succeeds once the collection exists.
    ///
    /// # Errors
    /// [`Error::NotFound`] or a backend error.
    pub async fn remove(&self, key: &str) -> Result<()> {
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::MacOs(ring) => ring.remove(key),
            #[cfg(all(unix, not(target_os = "macos")))]
            Backend::SecretService(ring) => ring.remove(key).await,
            Backend::File(ring) => ring.remove(key),
        }
    }
}
