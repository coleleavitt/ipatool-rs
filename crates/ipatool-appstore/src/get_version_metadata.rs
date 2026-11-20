//! Port of Go `appstore_get_version_metadata.go`: the display version and release date of
//! one version, read from its package rather than the download API's metadata.

use crate::download_package::{download_package_platform, metadata_string};
use crate::list_versions::{version_item, version_platform};
use crate::macos_version_metadata::read_version_metadata_from_mac_package;
use crate::partial_zip::VersionMetadata;
use crate::{Account, App, Error, Platform, Result, StoreSession, machine};

impl StoreSession<'_> {
    /// Go's `GetVersionMetadata`.
    ///
    /// # Errors
    /// [`Error::PasswordTokenExpired`], [`Error::LicenseRequired`] and Go's other errors.
    pub async fn get_version_metadata(
        &self,
        account: &Account,
        app: &App,
        version_id: &str,
        requested: Option<Platform>,
    ) -> Result<VersionMetadata> {
        let mut platform = version_platform(requested)?;
        let mac_address =
            (self.mac_address)().map_err(|error| error.context("failed to get mac address"))?;
        let guid = mac_address.to_uppercase().replace(':', "");

        let (response, _) = self
            .download_product(account, app, &guid, version_id, Some(platform))
            .await?;
        let item = version_item(&response)?;
        // A pinned Mac version can be returned even when no platform was supplied. Use the
        // response's package type instead of passing it to the IPA reader.
        if requested.is_none() {
            let software_platform = metadata_string(&item.metadata, "software-platform");
            if software_platform.eq_ignore_ascii_case("macos")
                || (software_platform.is_empty()
                    && metadata_string(&item.metadata, "product-type")
                        .eq_ignore_ascii_case("mac-os-app"))
            {
                platform = Platform::MacOs;
            }
        }

        if platform == Platform::MacOs
            && download_package_platform(Some(platform), item)? == Some(Platform::MacOs)
        {
            let (_, hardware_id) = machine::identity(&mac_address)?;
            return read_version_metadata_from_mac_package(
                &self.mac_package,
                item,
                &hardware_id,
                &app.bundle_id,
                async |url: &str, path: &std::path::Path| {
                    self.client.download_file(url, path, None).await
                },
            )
            .await
            .map_err(|error| error.context("failed to read macOS version metadata"));
        }

        // Do not fall back to the item's metadata: the download API can return stale
        // version and release date values, so the IPA's Info.plist is the source of truth.
        let client = self.client.clone();
        let url = item.url.clone();
        tokio::task::spawn_blocking(move || client.read_version_metadata_from_ipa(&url))
            .await
            .map_err(|error| Error::Invalid(format!("version metadata task failed: {error}")))?
            .map_err(|error| error.context("failed to read version metadata"))
    }
}
