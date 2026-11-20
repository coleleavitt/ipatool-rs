//! Port of Go `appstore_macos_version_metadata.go`: the display version and build date
//! of one macOS version, read from its package rather than the app-level API metadata.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::download::DownloadItem;
use crate::download_macos::MacPackageDownload;
use crate::download_package::{has_mac_dp_info, metadata_string};
use crate::macos_preflight_metadata::preflight_version_metadata;
use crate::xar::{self, ReadAt, go_path};
use crate::{Error, Result};

/// Go's `versionMetadata`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VersionMetadata {
    pub display_version: String,
    pub release_date: OffsetDateTime,
}

/// `xml.Unmarshal`'s input rules: UTF-8 text with an optional DTD.
pub(crate) fn xml_document(data: &[u8]) -> Result<roxmltree::Document<'_>> {
    let text = std::str::from_utf8(data)
        .map_err(|_| Error::Invalid("XML syntax error: invalid UTF-8".to_owned()))?;
    roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..roxmltree::ParsingOptions::default()
        },
    )
    .map_err(|err| Error::Invalid(format!("XML syntax error: {err}")))
}

/// Go's `readVersionMetadataFromMacPackage`. `download(url, path)` is Go's `downloadFile`.
pub(crate) async fn read_version_metadata_from_mac_package(
    job: &MacPackageDownload,
    item: &DownloadItem,
    hardware_id: &[u8],
    bundle_id: &str,
    download: impl AsyncFnOnce(&str, &Path) -> Result<()>,
) -> Result<VersionMetadata> {
    let directory = tempfile::Builder::new()
        .prefix("ipatool-version-metadata-")
        .tempdir()
        .map_err(|err| Error::Invalid(format!("failed to create metadata staging: {err}")))?;
    let destination: PathBuf = directory.path().join("app.pkg");
    // Older device-based downloads omit dpInfo but include a small, readable preflight
    // archive containing the selected build's installer metadata.
    let preflight = !has_mac_dp_info(&item.sinfs) && !item.preflight_package_url.is_empty();
    if preflight {
        download(&item.preflight_package_url, &destination)
            .await
            .map_err(|err| err.context("failed to download Mac preflight package"))?;
    } else {
        job.run(item, &destination, hardware_id, download).await?;
    }
    let bundle_id = if bundle_id.is_empty() {
        metadata_string(&item.metadata, "softwareVersionBundleId")
    } else {
        bundle_id
    };
    let archive = xar::Reader::open(&destination)
        .map_err(|err| Error::Invalid(format!("failed to open Mac package: {err}")))?;
    if preflight {
        preflight_version_metadata(&archive, bundle_id)
    } else {
        package_version_metadata(&archive, bundle_id)
    }
}

/// Go's `macPackageVersionMetadata`: the matching bundle's `PackageInfo` version and its
/// sibling `Payload` timestamp.
pub(crate) fn package_version_metadata<R: ReadAt>(
    archive: &xar::Reader<R>,
    bundle_id: &str,
) -> Result<VersionMetadata> {
    let mut result = None;
    for file in archive
        .files
        .values()
        .filter(|file| go_path::base(&file.name) == "PackageInfo")
    {
        let data = archive.read_file(file).map_err(|err| {
            let action = if matches!(err, xar::Error::FileEncodingUnsupported) {
                "open"
            } else {
                "read"
            };
            Error::Invalid(format!("failed to {action} PackageInfo: {err}"))
        })?;
        let document =
            xml_document(&data).map_err(|err| err.context("failed to parse PackageInfo"))?;
        let bundles = document
            .root_element()
            .children()
            .filter(|node| node.has_tag_name("bundle"));
        for bundle in bundles {
            let id = bundle.attribute("id").unwrap_or_default();
            if !bundle_id.is_empty() && id != bundle_id {
                continue;
            }
            let version = bundle
                .attribute("CFBundleShortVersionString")
                .unwrap_or_default()
                .trim();
            if version.is_empty() {
                return Err(Error::Invalid(
                    "macOS package does not contain a display version".to_owned(),
                ));
            }
            if result.is_some() {
                return Err(Error::Invalid(
                    "macOS package contains multiple matching bundles".to_owned(),
                ));
            }
            // Like the IPA Info.plist timestamp fallback, use the selected build's archived
            // payload timestamp, not the API's app-level release date.
            let payload = go_path::join(&go_path::dir(&file.name), "Payload");
            let modified = archive
                .files
                .values()
                .find(|entry| entry.name == payload)
                .map_or(0, |entry| entry.info.mtime);
            let release_date = (modified > 0)
                .then(|| OffsetDateTime::from_unix_timestamp(modified).ok())
                .flatten()
                .ok_or_else(|| {
                    Error::Invalid("macOS package does not contain a payload timestamp".to_owned())
                })?;
            result = Some(VersionMetadata {
                display_version: version.to_owned(),
                release_date,
            });
        }
    }
    result.ok_or_else(|| {
        Error::Invalid(
            "macOS package does not contain metadata for the requested bundle".to_owned(),
        )
    })
}

#[cfg(test)]
mod tests;
