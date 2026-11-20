//! Go's `appstore_download_package.go`. Placeholder until the macOS port (branch
//! `phase6-macos`) replaces this file.

use crate::download_types::DownloadItem;
use crate::{Error, Platform, Result};

/// Go's `downloadPackagePlatform`.
pub(crate) fn download_package_platform(
    platform: Option<Platform>,
    _item: &DownloadItem,
) -> Result<Option<Platform>> {
    if platform == Some(Platform::MacOs) {
        return Err(Error::NotPorted("macOS package selection"));
    }
    Ok(platform)
}
