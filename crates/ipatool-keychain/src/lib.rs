//! Credential storage compatible with the Go ipatool, which uses
//! `github.com/byteness/keyring`. The same service name and keys give access to the
//! same entries from either tool:
//!
//! - macOS and iOS: a generic password per key in the Keychain (service = service name,
//!   account = key);
//! - Linux: a Secret Service collection named after the service, one item per key
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
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod macos;
#[cfg(any(target_os = "linux", target_os = "android"))]
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
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    MacOs(macos::MacKeychain),
    #[cfg(any(target_os = "linux", target_os = "android"))]
    SecretService(secret_service::SecretServiceKeyring),
    File(file::FileKeyring),
}

// Only the Secret Service backend awaits; the API stays async on every platform.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android")),
    allow(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "async only for the Secret Service backend"
    )
)]
impl Keychain {
    /// Opens the first available backend, in the Go order: macOS Keychain, Secret
    /// Service, encrypted files.
    pub async fn open(config: Config) -> Self {
        let label = config.service_name.clone();
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            let _ = &config.file_dir;
            return Self {
                backend: Backend::MacOs(macos::MacKeychain::new(config.service_name)),
                label,
            };
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some(ring) = secret_service::SecretServiceKeyring::open(&config.service_name).await {
            return Self {
                backend: Backend::SecretService(ring),
                label,
            };
        }
        #[allow(unreachable_code, reason = "Apple targets always return above")]
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
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Backend::MacOs(_) => "keychain",
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Backend::SecretService(_) => "secret-service",
            Backend::File(_) => "file",
        }
    }

    /// Errors read `failed to get item: …`, as Go's `keychain.Get` wraps them.
    ///
    /// # Errors
    /// [`Error::is_not_found`] when the key is not stored, or a backend error.
    pub async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let result = match &self.backend {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Backend::MacOs(ring) => ring.get(key),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Backend::SecretService(ring) => ring.get(key).await,
            Backend::File(ring) => ring.get(key),
        };
        result.map_err(Error::op("get"))
    }

    /// # Errors
    /// A backend error, as `failed to set item: …`.
    pub async fn set(&self, key: &str, data: &[u8]) -> Result<()> {
        let item = item::StoredItem::new(key, data, &self.label);
        let result = match &self.backend {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Backend::MacOs(ring) => ring.set(&item),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Backend::SecretService(ring) => ring.set(&item).await,
            Backend::File(ring) => ring.set(&item),
        };
        result.map_err(Error::op("set"))
    }

    /// Like Go, removing a missing key is not found except on Secret Service, where it
    /// succeeds once the collection exists.
    ///
    /// # Errors
    /// [`Error::is_not_found`] or a backend error, as `failed to remove item: …`.
    pub async fn remove(&self, key: &str) -> Result<()> {
        let result = match &self.backend {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Backend::MacOs(ring) => ring.remove(key),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Backend::SecretService(ring) => ring.remove(key).await,
            Backend::File(ring) => ring.remove(key),
        };
        result.map_err(Error::op("remove"))
    }
}
