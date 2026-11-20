use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use sha1::{Digest, Sha1};
use time::macros::datetime;

use super::*;
use crate::download::Sinf;
use crate::download_macos::{MacPackageDecrypter, MacPackageDownload, OsFileSystem};
use crate::macos_preflight_metadata::tests::{preflight_distribution, preflight_xar};
use crate::xar::tests::{hex, test_archive};

const INFO: &str =
    r#"<pkg-info><bundle id="com.example.mac" CFBundleShortVersionString="1.2.1"/></pkg-info>"#;
const MODIFIED: &str = "2025-07-24T08:38:42Z";

/// Go's `macMetadataXAR`.
fn metadata_xar(info: &str, modified: &str) -> Vec<u8> {
    let payload = b"app payload";
    let mut entries = String::new();
    let mut offset = 20;
    for (index, (name, data)) in [("PackageInfo", info.as_bytes()), ("Payload", payload)]
        .into_iter()
        .enumerate()
    {
        let sum = hex(&Sha1::digest(data));
        let _ = write!(
            entries,
            "<file id=\"{id}\"><type>file</type><name>{name}</name><mtime>{modified}</mtime><data><length>{len}</length><offset>{offset}</offset><size>{len}</size><encoding style=\"application/octet-stream\"/><archived-checksum style=\"sha1\">{sum}</archived-checksum><extracted-checksum style=\"sha1\">{sum}</extracted-checksum></data></file>",
            id = index + 1,
            len = data.len()
        );
        offset += data.len();
    }
    let mut heap = info.as_bytes().to_vec();
    heap.extend_from_slice(payload);
    test_archive(&entries, &heap)
}

#[test]
fn validates_package_metadata() {
    let cases: &[(&str, &str, &str, &str, bool)] = &[
        ("matching bundle", INFO, "com.example.mac", MODIFIED, true),
        (
            "ID-only lookup with a unique bundle",
            INFO,
            "",
            MODIFIED,
            true,
        ),
        ("wrong bundle", INFO, "com.other.mac", MODIFIED, false),
        (
            "missing version",
            r#"<pkg-info><bundle id="com.example.mac"/></pkg-info>"#,
            "com.example.mac",
            MODIFIED,
            false,
        ),
        ("missing timestamp", INFO, "com.example.mac", "", false),
        (
            "ambiguous bundles",
            r#"<pkg-info><bundle id="a" CFBundleShortVersionString="1"/><bundle id="b" CFBundleShortVersionString="2"/></pkg-info>"#,
            "",
            MODIFIED,
            false,
        ),
        ("invalid XML", "<", "com.example.mac", MODIFIED, false),
    ];
    for (name, contents, bundle, stamp, success) in cases {
        let data = metadata_xar(contents, stamp);
        let size = data.len() as u64;
        let archive = xar::Reader::new(data, size).expect("valid XAR");
        let result = package_version_metadata(&archive, bundle);
        if *success {
            let metadata = result.unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(metadata.display_version, "1.2.1", "{name}");
            assert_eq!(
                metadata.release_date,
                datetime!(2025-07-24 08:38:42 UTC),
                "{name}"
            );
        } else {
            assert!(result.is_err(), "{name}");
        }
    }
}

struct Decrypter(Vec<u8>);

impl MacPackageDecrypter for Decrypter {
    fn decrypt(&mut self, dst: &mut dyn std::io::Write, _: &mut dyn std::io::Read) -> Result<u64> {
        dst.write_all(&self.0)
            .map_err(|err| Error::Invalid(err.to_string()))?;
        Ok(self.0.len() as u64)
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

fn job(output: Vec<u8>, calls: Arc<Mutex<usize>>) -> MacPackageDownload {
    MacPackageDownload {
        fs: Arc::new(OsFileSystem),
        factory: Arc::new(move |hardware_id, dp_info| {
            assert_eq!(hardware_id, [0, 17, 34, 51, 68, 85]);
            assert_eq!(dp_info, b"dpInfo");
            *calls.lock().expect("calls") += 1;
            Ok(Box::new(Decrypter(output.clone())))
        }),
    }
}

fn metadata_map(pairs: &[(&str, &str)]) -> plist::Dictionary {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), plist::Value::from(*value)))
        .collect()
}

/// The staging directory of the one download, which must be gone afterwards.
type Downloads = Mutex<Vec<(String, PathBuf)>>;

#[tokio::test]
async fn decrypts_the_selected_mac_version_and_ignores_stale_api_metadata() {
    let calls = Arc::default();
    let job = job(metadata_xar(INFO, MODIFIED), Arc::clone(&calls));
    let item = DownloadItem {
        url: "https://example.test/encrypted.pkg".to_owned(),
        sinfs: vec![Sinf {
            dp_info: b"dpInfo".to_vec(),
            ..Sinf::default()
        }],
        metadata: metadata_map(&[
            ("software-platform", "macos"),
            ("bundleShortVersionString", "9.9"),
            ("releaseDate", "2023-12-26T08:00:00Z"),
        ]),
        ..DownloadItem::default()
    };
    let downloads = Downloads::default();
    let out = read_version_metadata_from_mac_package(
        &job,
        &item,
        &[0, 17, 34, 51, 68, 85],
        "com.example.mac",
        async |url: &str, path: &Path| {
            downloads
                .lock()
                .expect("downloads")
                .push((url.to_owned(), path.to_path_buf()));
            std::fs::write(path, b"encrypted").map_err(|err| Error::Invalid(err.to_string()))
        },
    )
    .await
    .expect("metadata");
    assert_eq!(out.display_version, "1.2.1");
    assert_eq!(out.release_date, datetime!(2025-07-24 08:38:42 UTC));
    assert_eq!(*calls.lock().expect("calls"), 1);
    let downloads = downloads.into_inner().expect("downloads");
    assert_eq!(downloads.len(), 1);
    assert_eq!(downloads[0].0, "https://example.test/encrypted.pkg");
    let staging = downloads[0].1.parent().expect("staging directory");
    assert!(
        staging
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("ipatool-version-metadata-"))
    );
    assert!(!staging.exists());
}

#[tokio::test]
async fn reads_the_selected_legacy_version_without_downloading_the_encrypted_app() {
    let cases = [
        ("macos", "4.0.5", "4.0.5.0.1.1292967101", 1_292_967_101),
        ("macos", "4.2", "4.2.0.0.1.1341568473", 1_341_568_473),
        ("", "4.2", "4.2.0.0.1.1341568473", 1_341_568_473),
    ];
    for (software_platform, display_version, package_version, timestamp) in cases {
        let calls = Arc::default();
        let job = job(Vec::new(), Arc::clone(&calls));
        let data = preflight_xar(&preflight_distribution(display_version, package_version));
        let item = DownloadItem {
            url: "https://example.test/encrypted.pkg".to_owned(),
            preflight_package_url: "https://example.test/preflight.pkg".to_owned(),
            metadata: metadata_map(&[
                ("software-platform", software_platform),
                ("product-type", "mac-os-app"),
                ("softwareVersionBundleId", "com.example.mac"),
                ("bundleShortVersionString", "4.3"),
                ("releaseDate", "2011-01-03T08:00:00Z"),
            ]),
            ..DownloadItem::default()
        };
        let downloads = Downloads::default();
        let out = read_version_metadata_from_mac_package(
            &job,
            &item,
            &[0, 17, 34, 51, 68, 85],
            "",
            async |url: &str, path: &Path| {
                downloads
                    .lock()
                    .expect("downloads")
                    .push((url.to_owned(), path.to_path_buf()));
                std::fs::write(path, &data).map_err(|err| Error::Invalid(err.to_string()))
            },
        )
        .await
        .expect("metadata");
        assert_eq!(out.display_version, display_version);
        assert_eq!(
            out.release_date,
            OffsetDateTime::from_unix_timestamp(timestamp).expect("time")
        );
        assert_eq!(*calls.lock().expect("calls"), 0);
        let downloads = downloads.into_inner().expect("downloads");
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].0, "https://example.test/preflight.pkg");
        assert!(!downloads[0].1.parent().expect("staging").exists());
    }
}

#[tokio::test]
async fn reports_a_failed_preflight_download() {
    let job = job(Vec::new(), Arc::default());
    let item = DownloadItem {
        preflight_package_url: "https://example.test/preflight.pkg".to_owned(),
        ..DownloadItem::default()
    };
    let err =
        read_version_metadata_from_mac_package(&job, &item, &[], "", async |_: &str, _: &Path| {
            Err(Error::Invalid("HTTP 500".to_owned()))
        })
        .await
        .expect_err("download error");
    assert_eq!(
        err.to_string(),
        "failed to download Mac preflight package: HTTP 500"
    );
}
