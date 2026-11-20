//! Port of Go `appstore_download_package.go`: which package a macOS download response holds.

use crate::download::{DownloadItem, Sinf};
use crate::{Error, Platform, Result};

/// Go's `downloadPackagePlatform`: a macOS request can return an iOS app made available
/// on the Mac (an `.ipa` with mobile sinfs) or a native `.pkg` (with `dpInfo`).
pub(crate) fn download_package_platform(
    platform: Option<Platform>,
    item: &DownloadItem,
) -> Result<Option<Platform>> {
    if platform != Some(Platform::MacOs) {
        return Ok(platform);
    }
    let software_platform = metadata_string(&item.metadata, "software-platform");
    let product_type = metadata_string(&item.metadata, "product-type");
    // Universal Mac apps can retain product-type=ios-app. Prefer the package's explicit
    // software platform over that catalog classification.
    let (mobile_metadata, mac_metadata) = if software_platform.is_empty() {
        (
            product_type.eq_ignore_ascii_case("ios-app"),
            product_type.eq_ignore_ascii_case("mac-os-app"),
        )
    } else {
        (
            software_platform.eq_ignore_ascii_case("ios"),
            software_platform.eq_ignore_ascii_case("macos"),
        )
    };
    let mobile_sinf = has_mobile_sinf(&item.sinfs);
    let mobile_sinf_evidence = has_mobile_sinf_evidence(&item.sinfs);
    let mac_dp_info = has_mac_dp_info(&item.sinfs);
    if (mobile_metadata && mac_dp_info)
        || (mac_metadata && mobile_sinf_evidence)
        || (mobile_sinf_evidence && mac_dp_info)
    {
        return Err(Error::Invalid(
            "download response contains conflicting package metadata".to_owned(),
        ));
    }
    if mobile_metadata || mobile_sinf_evidence {
        if !mobile_sinf {
            return Err(Error::Invalid(
                "download response does not contain mobile sinf data".to_owned(),
            ));
        }
        return Ok(Some(Platform::IPhone));
    }
    Ok(Some(Platform::MacOs))
}

/// Go's `downloadMetadataString`: the value when it is a string, else empty.
pub(crate) fn metadata_string<'a>(metadata: &'a plist::Dictionary, key: &str) -> &'a str {
    metadata
        .get(key)
        .and_then(plist::Value::as_string)
        .unwrap_or_default()
}

fn has_mobile_sinf(sinfs: &[Sinf]) -> bool {
    !sinfs.is_empty()
        && sinfs
            .iter()
            .all(|sinf| !sinf.data.is_empty() && sinf.dp_info.is_empty())
}

fn has_mobile_sinf_evidence(sinfs: &[Sinf]) -> bool {
    sinfs
        .iter()
        .any(|sinf| !sinf.data.is_empty() && sinf.dp_info.is_empty())
}

pub(crate) fn has_mac_dp_info(sinfs: &[Sinf]) -> bool {
    sinfs.iter().any(|sinf| !sinf.dp_info.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(metadata: &[(&str, &str)], sinfs: Vec<Sinf>) -> DownloadItem {
        DownloadItem {
            metadata: metadata
                .iter()
                .map(|(key, value)| ((*key).to_owned(), plist::Value::from(*value)))
                .collect(),
            sinfs,
            ..DownloadItem::default()
        }
    }

    fn sinf(data: &[u8], dp_info: &[u8]) -> Sinf {
        Sinf {
            data: data.to_vec(),
            dp_info: dp_info.to_vec(),
            ..Sinf::default()
        }
    }

    const NATIVE: &[(&str, &str)] = &[
        ("software-platform", "macos"),
        ("product-type", "mac-os-app"),
    ];
    const MAC: Option<Platform> = Some(Platform::MacOs);

    fn error(platform: Option<Platform>, item: &DownloadItem) -> String {
        download_package_platform(platform, item)
            .expect_err("platform error")
            .to_string()
    }

    #[test]
    fn uses_the_requested_platform_for_native_macos_packages() {
        let item = item(NATIVE, vec![sinf(b"", b"dp info")]);
        assert_eq!(download_package_platform(MAC, &item).ok(), Some(MAC));
    }

    #[test]
    fn recognizes_universal_mac_packages_with_an_ios_product_type() {
        let item = item(
            &[("software-platform", "macos"), ("product-type", "ios-app")],
            vec![sinf(b"", &[0; 88])],
        );
        assert_eq!(download_package_platform(MAC, &item).ok(), Some(MAC));
    }

    #[test]
    fn accepts_native_authorization_data_that_also_contains_sinf_data() {
        let item = item(NATIVE, vec![sinf(b"sinf", b"dp info")]);
        assert_eq!(download_package_platform(MAC, &item).ok(), Some(MAC));
    }

    #[test]
    fn recognizes_ios_apps_made_available_on_macos() {
        let item = item(
            &[("software-platform", "ios"), ("product-type", "ios-app")],
            vec![sinf(b"sinf", b"")],
        );
        assert_eq!(
            download_package_platform(MAC, &item).ok(),
            Some(Some(Platform::IPhone))
        );
    }

    #[test]
    fn uses_mobile_sinf_data_when_platform_metadata_is_absent() {
        let item = item(&[], vec![sinf(b"sinf", b"")]);
        assert_eq!(
            download_package_platform(MAC, &item).ok(),
            Some(Some(Platform::IPhone))
        );
    }

    #[test]
    fn rejects_mobile_metadata_without_usable_sinf_data() {
        let item = item(&[("software-platform", "ios")], Vec::new());
        assert_eq!(
            error(MAC, &item),
            "download response does not contain mobile sinf data"
        );
    }

    #[test]
    fn rejects_conflicting_platform_metadata() {
        let item = item(
            &[("software-platform", "macos"), ("product-type", "ios-app")],
            vec![sinf(b"sinf", b"")],
        );
        assert_eq!(
            error(MAC, &item),
            "download response contains conflicting package metadata"
        );
    }

    #[test]
    fn rejects_conflicting_authorization_data() {
        let item = item(&[], vec![sinf(b"sinf", b""), sinf(b"", b"dp info")]);
        assert_eq!(
            error(MAC, &item),
            "download response contains conflicting package metadata"
        );
    }

    #[test]
    fn does_not_override_non_macos_requests() {
        let item = item(NATIVE, Vec::new());
        let ipad = Some(Platform::IPad);
        assert_eq!(download_package_platform(ipad, &item).ok(), Some(ipad));
        assert_eq!(download_package_platform(None, &item).ok(), Some(None));
    }

    #[test]
    fn metadata_strings_ignore_other_types() {
        let mut metadata = plist::Dictionary::new();
        metadata.insert("software-platform".to_owned(), plist::Value::from(1));
        assert_eq!(metadata_string(&metadata, "software-platform"), "");
        assert_eq!(metadata_string(&metadata, "missing"), "");
    }
}
