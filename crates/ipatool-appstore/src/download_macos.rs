//! Go's `appstore_download_macos.go`. Placeholder until the macOS port (branch
//! `phase6-macos`) replaces this file.

use std::path::Path;

use crate::download_types::{DownloadItem, DownloadOutput, ProgressHandle};
use crate::{Client, Error, Result};

impl Client {
    /// Go's `downloadMacPackage`.
    ///
    /// # Errors
    /// Not ported in this branch.
    #[allow(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "the macOS port fills this in"
    )]
    pub(crate) async fn download_mac_package(
        &self,
        _item: &DownloadItem,
        _destination: &Path,
        _hardware_id: &[u8],
        _progress: Option<ProgressHandle>,
    ) -> Result<DownloadOutput> {
        Err(Error::NotPorted("macOS package download"))
    }
}
