//! Replays `tests/fixtures/macos-package-oracle.json`, recorded from Go by
//! `scripts/macos-package-oracle.sh`.

use base64::Engine as _;
use serde::Deserialize;

use crate::download::{DownloadItem, Sinf};
use crate::download_package::download_package_platform;
use crate::macos_preflight_metadata::preflight_version_metadata;
use crate::macos_version_lookup::macos_external_version_id;
use crate::macos_version_metadata::package_version_metadata;
use crate::xar::{self, ReadError};
use crate::{App, Platform};

#[derive(Deserialize)]
struct Oracle {
    xar: Vec<XarCase>,
    metadata: Vec<MetadataCase>,
    offers: Vec<OfferCase>,
    platforms: Vec<PlatformCase>,
}

#[derive(Deserialize)]
struct XarCase {
    name: String,
    #[serde(deserialize_with = "bytes")]
    input: Vec<u8>,
    #[serde(default)]
    error: String,
    files: Vec<XarFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct XarFile {
    id: u64,
    name: String,
    mtime: i64,
    mode: u32,
    uid: i64,
    gid: i64,
    verify: bool,
    #[serde(default, deserialize_with = "bytes")]
    content: Vec<u8>,
    #[serde(default)]
    open_error: String,
    #[serde(default)]
    read_error: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataCase {
    kind: String,
    name: String,
    #[serde(deserialize_with = "bytes")]
    input: Vec<u8>,
    bundle: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    display_version: String,
    #[serde(default)]
    release_date: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OfferCase {
    name: String,
    body: String,
    app_id: i64,
    bundle_id: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    error: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OracleSinf {
    #[serde(default, deserialize_with = "bytes")]
    data: Vec<u8>,
    #[serde(default, deserialize_with = "bytes")]
    dp_info: Vec<u8>,
}

#[derive(Deserialize)]
struct PlatformCase {
    platform: String,
    metadata: std::collections::BTreeMap<String, String>,
    sinfs: Option<Vec<OracleSinf>>,
    #[serde(default)]
    result: String,
    #[serde(default)]
    error: String,
}

/// Go's `[]byte` JSON: base64, or `null`.
fn bytes<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let text: Option<String> = Option::deserialize(deserializer)?;
    text.map_or_else(
        || Ok(Vec::new()),
        |text| {
            base64::engine::general_purpose::STANDARD
                .decode(text)
                .map_err(serde::de::Error::custom)
        },
    )
}

fn oracle() -> Oracle {
    serde_json::from_str(include_str!("../tests/fixtures/macos-package-oracle.json"))
        .expect("oracle fixture")
}

/// Texts produced by another library: Go's `time.Parse` and XML parser details differ, so
/// only the part up to the library's own detail must match.
fn same_error(go: &str, rust: &str) -> bool {
    const DETAILED: [&str; 2] = [
        "as \"2006-01-02T15:04:05Z07:00\"",
        "XML syntax error on line ",
    ];
    if go == rust {
        return true;
    }
    DETAILED.iter().any(|marker| {
        go.find(marker).is_some_and(|at| {
            let end = at + marker.len() + usize::from(marker.ends_with(' '));
            rust.get(..end) == go.get(..end)
        })
    })
}

#[test]
fn xar_reader_matches_go() {
    for case in oracle().xar {
        let size = case.input.len() as u64;
        let name = &case.name;
        match xar::Reader::new(case.input, size) {
            Err(err) => assert!(
                same_error(&case.error, &err.to_string()),
                "{name}: Go {:?}, Rust {err}",
                case.error
            ),
            Ok(archive) => {
                assert_eq!(case.error, "", "{name}: Rust accepted the archive");
                let ids: Vec<u64> = archive.files.keys().copied().collect();
                let go_ids: Vec<u64> = case.files.iter().map(|file| file.id).collect();
                assert_eq!(ids, go_ids, "{name}");
                for expected in &case.files {
                    let file = &archive.files[&expected.id];
                    assert_eq!(file.name, expected.name, "{name}");
                    assert_eq!(file.info.mtime, expected.mtime, "{name}");
                    assert_eq!(file.info.mode, expected.mode, "{name}");
                    assert_eq!(file.info.uid, expected.uid, "{name}");
                    assert_eq!(file.info.gid, expected.gid, "{name}");
                    assert_eq!(archive.verify_checksum(file), expected.verify, "{name}");
                    match archive.read_file(file) {
                        Ok(content) => {
                            assert_eq!(
                                (expected.open_error.as_str(), expected.read_error.as_str()),
                                ("", ""),
                                "{name}"
                            );
                            assert_eq!(content, expected.content, "{name}");
                        }
                        Err(ReadError::Open(err)) => {
                            assert_eq!(err.to_string(), expected.open_error, "{name}");
                        }
                        Err(ReadError::Read(err)) => {
                            assert_eq!(err.to_string(), expected.read_error, "{name}");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn version_metadata_matches_go() {
    for case in oracle().metadata {
        let name = format!("{} {}", case.kind, case.name);
        let size = case.input.len() as u64;
        let archive = xar::Reader::new(case.input, size).expect("oracle archive");
        let result = if case.kind == "preflight" {
            preflight_version_metadata(&archive, &case.bundle)
        } else {
            package_version_metadata(&archive, &case.bundle)
        };
        match result {
            Ok(metadata) => {
                assert_eq!(case.error, "", "{name}");
                assert_eq!(metadata.display_version, case.display_version, "{name}");
                assert_eq!(
                    metadata.release_date.unix_timestamp(),
                    case.release_date,
                    "{name}"
                );
            }
            Err(err) => assert!(
                same_error(&case.error, &err.to_string()),
                "{name}: Go {:?}, Rust {err}",
                case.error
            ),
        }
    }
}

#[test]
fn mac_offer_selection_matches_go() {
    for case in oracle().offers {
        let app = App {
            id: case.app_id,
            bundle_id: case.bundle_id.clone(),
            name: String::new(),
            version: String::new(),
            price: 0.0,
            platforms: Vec::new(),
        };
        let name = format!("{} {} {:?}", case.name, case.app_id, case.bundle_id);
        match macos_external_version_id(case.body.as_bytes(), &app) {
            Ok(version) => assert_eq!(
                (version.as_str(), ""),
                (case.version.as_str(), case.error.as_str()),
                "{name}"
            ),
            // serde_json and encoding/json word decode errors differently.
            Err(err)
                if case
                    .error
                    .starts_with("failed to decode Mac product page: ") =>
            {
                assert!(
                    err.to_string()
                        .starts_with("failed to decode Mac product page: "),
                    "{name}: {err}"
                );
            }
            Err(err) => assert_eq!(err.to_string(), case.error, "{name}"),
        }
    }
}

fn platform(text: &str) -> Option<Platform> {
    [
        Platform::IPhone,
        Platform::IPad,
        Platform::AppleTv,
        Platform::WatchOs,
        Platform::VisionOs,
        Platform::MacOs,
    ]
    .into_iter()
    .find(|platform| platform.as_str() == text)
}

#[test]
fn package_platform_matches_go() {
    for case in oracle().platforms {
        let item = DownloadItem {
            metadata: case
                .metadata
                .iter()
                .map(|(key, value)| (key.clone(), plist::Value::from(value.as_str())))
                .collect(),
            sinfs: case
                .sinfs
                .unwrap_or_default()
                .into_iter()
                .map(|sinf| Sinf {
                    data: sinf.data,
                    dp_info: sinf.dp_info,
                    ..Sinf::default()
                })
                .collect(),
            ..DownloadItem::default()
        };
        let requested = platform(&case.platform);
        assert_eq!(
            requested.is_none(),
            case.platform.is_empty(),
            "{}",
            case.platform
        );
        let name = format!("{} {:?}", case.platform, case.metadata);
        match download_package_platform(requested, &item) {
            Ok(result) => assert_eq!(
                (result.map_or("", Platform::as_str), ""),
                (case.result.as_str(), case.error.as_str()),
                "{name}"
            ),
            Err(err) => assert_eq!(err.to_string(), case.error, "{name}"),
        }
    }
}

/// A real decrypted package from the Go tool, e.g. `ipatool download --platform macos`:
/// `IPATOOL_TEST_MAC_PKG=<path> IPATOOL_TEST_MAC_BUNDLE=<bundle id> cargo test -p
/// ipatool-appstore real_mac_package`.
#[test]
fn real_mac_package() {
    let Ok(path) = std::env::var("IPATOOL_TEST_MAC_PKG") else {
        return;
    };
    let bundle = std::env::var("IPATOOL_TEST_MAC_BUNDLE").unwrap_or_default();
    let archive = xar::Reader::open(std::path::Path::new(&path)).expect("XAR");
    assert!(!archive.files.is_empty());
    for file in archive.files.values() {
        assert!(archive.verify_checksum(file), "{}", file.name);
    }
    // Go's PackageInfo reader only sees `bundle` elements directly under the root; current
    // Mac App Store packages nest them in `bundle-version`, so Go (and this port) report
    // "does not contain metadata for the requested bundle" for them.
    let metadata = package_version_metadata(&archive, &bundle);
    eprintln!("{path}: {} files, {metadata:?}", archive.files.len());
}
