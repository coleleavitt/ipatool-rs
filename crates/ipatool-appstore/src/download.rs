//! Download types. Placeholder with the shapes agreed with the phase-6 iOS port
//! (branch `phase6-ios`), which owns this module; replaced on merge.

use std::path::PathBuf;

/// Go's `Sinf` (`appstore_replicate_sinf.go`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sinf {
    pub id: i64,
    pub data: Vec<u8>,
    pub dp_info: Vec<u8>,
}

/// Go's `downloadItemResult`.
#[derive(Clone, Debug, Default)]
pub struct DownloadItem {
    pub preflight_package_url: String,
    pub artwork_url: String,
    pub hash_md5: String,
    pub url: String,
    pub sinfs: Vec<Sinf>,
    pub metadata: plist::Dictionary,
}

/// Go's `DownloadOutput`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadOutput {
    pub destination_path: PathBuf,
    pub sinfs: Vec<Sinf>,
}

/// Download progress.
pub trait Progress: Send + Sync {
    fn set_total(&self, total: i64);
    fn set(&self, position: i64);
    fn add(&self, bytes: u64);
}
