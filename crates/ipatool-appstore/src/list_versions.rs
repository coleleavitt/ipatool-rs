//! Port of Go `appstore_list_versions.go`: the external version identifiers of an app, from
//! the download response's metadata.

use crate::download_product::{failure, store_error};
use crate::download_types::sprint;
use crate::{Account, App, DownloadItem, DownloadProduct, Error, Platform, Result, StoreSession};

/// Go's `ListVersionsOutput`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListVersionsOutput {
    pub external_version_identifiers: Vec<String>,
    pub latest_external_version_id: String,
}

/// Go's platform check of `ListVersions` and `GetVersionMetadata`: iPhone when none is given.
pub(crate) fn version_platform(platform: Option<Platform>) -> Result<Platform> {
    match platform.unwrap_or(Platform::IPhone) {
        Platform::Unknown => Err(Error::InvalidPlatform("unknown".to_owned())),
        platform => Ok(platform),
    }
}

/// The failure checks `ListVersions` and `GetVersionMetadata` share: the first item.
pub(crate) fn version_item(response: &DownloadProduct) -> Result<&DownloadItem> {
    let data = &response.data;
    if data.failure_type == failure::PASSWORD_TOKEN_EXPIRED
        || data.failure_type == failure::SIGN_IN_REQUIRED
    {
        return Err(Error::PasswordTokenExpired);
    }
    if data.failure_type == failure::LICENSE_NOT_FOUND {
        return Err(Error::LicenseRequired);
    }
    if !data.customer_message.is_empty() && (!data.failure_type.is_empty() || data.items.is_empty())
    {
        return Err(store_error(
            format!("received error: {}", data.customer_message),
            response,
        ));
    }
    if !data.failure_type.is_empty() {
        return Err(store_error(
            format!("received error: {}", data.failure_type),
            response,
        ));
    }
    data.items
        .first()
        .ok_or_else(|| store_error("invalid response", response))
}

impl StoreSession<'_> {
    /// Go's `ListVersions`.
    ///
    /// # Errors
    /// [`Error::PasswordTokenExpired`], [`Error::LicenseRequired`] and Go's other errors.
    pub async fn list_versions(
        &self,
        account: &Account,
        app: &App,
        platform: Option<Platform>,
    ) -> Result<ListVersionsOutput> {
        let platform = version_platform(platform)?;
        let mac_address =
            (self.mac_address)().map_err(|error| error.context("failed to get mac address"))?;
        let guid = mac_address.to_uppercase().replace(':', "");
        let external_version_id = match platform {
            Platform::MacOs => self
                .client
                .lookup_latest_macos_external_version_id(account, app)
                .await
                .map(Some),
            Platform::AppleTv | Platform::WatchOs | Platform::VisionOs => self
                .client
                .lookup_latest_external_version_id(account, app, platform)
                .await
                .map(Some),
            _ => Ok(None),
        }
        .map_err(|error| error.context("failed to resolve platform version"))?
        .unwrap_or_default();

        let (response, _) = self
            .download_product(account, app, &guid, &external_version_id, Some(platform))
            .await?;
        let item = version_item(&response)?;
        let Some(plist::Value::Array(identifiers)) =
            item.metadata.get("softwareVersionExternalIdentifiers")
        else {
            return Err(Error::Invalid(
                "failed to get version identifiers from item metadata".to_owned(),
            ));
        };
        let Some(latest) = item.metadata.get("softwareVersionExternalIdentifier") else {
            return Err(Error::Invalid(
                "failed to get latest version from item metadata".to_owned(),
            ));
        };
        Ok(ListVersionsOutput {
            external_version_identifiers: identifiers
                .iter()
                .map(|identifier| sprint(Some(identifier)))
                .collect(),
            latest_external_version_id: sprint(Some(latest)),
        })
    }
}
