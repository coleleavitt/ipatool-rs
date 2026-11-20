use sha1::{Digest, Sha1};
use time::OffsetDateTime;

use super::*;
use crate::xar::tests::{hex, test_archive};

/// Go's `macPreflightXAR`.
pub(crate) fn preflight_xar(distribution: &str) -> Vec<u8> {
    let data = distribution.as_bytes();
    let sum = hex(&Sha1::digest(data));
    let entry = format!(
        "<file id=\"1\"><type>file</type><name>Distribution</name><data><length>{len}</length><offset>20</offset><size>{len}</size><encoding style=\"application/octet-stream\"/><archived-checksum style=\"sha1\">{sum}</archived-checksum><extracted-checksum style=\"sha1\">{sum}</extracted-checksum></data></file>",
        len = data.len()
    );
    // The live preflight packages were repacked in 2023. Their creation time must not
    // replace the original build timestamp in the installer version.
    test_archive(
        &format!("<creation-time>2023-01-04T22:53:15</creation-time>{entry}"),
        data,
    )
}

/// Go's `macPreflightDistribution`.
pub(crate) fn preflight_distribution(version: &str, package_version: &str) -> String {
    format!(
        "<installer-gui-script><product id=\"com.example.mac\" version=\"{version}\"/>\n<pkg-ref id=\"com.example.pkg\"><relocate><bundle id=\"com.example.mac\"/></relocate></pkg-ref>\n<pkg-ref id=\"com.unrelated.pkg\" version=\"9.9.9.0.1.1700000000\"/>\n<pkg-ref id=\"com.example.pkg\" version=\"{package_version}\"/>\n<pkg-ref id=\"com.example.pkg\" version=\"{package_version}\"/></installer-gui-script>"
    )
}

fn metadata(data: Vec<u8>, bundle: &str) -> Result<VersionMetadata> {
    let size = data.len() as u64;
    let archive = xar::Reader::new(data, size).expect("valid XAR");
    preflight_version_metadata(&archive, bundle)
}

#[test]
fn validates_the_selected_product_and_its_build_timestamp() {
    let good = preflight_distribution("4.2", "4.2.0.0.1.1341568473");
    let cases: &[(&str, String, &str, &str)] = &[
        (
            "unique product without a bundle filter",
            good.clone(),
            "",
            "",
        ),
        (
            "wrong bundle",
            good.clone(),
            "com.other.mac",
            "requested bundle",
        ),
        (
            "missing version",
            preflight_distribution("", "4.2.0.0.1.1341568473"),
            "",
            "display version",
        ),
        (
            "missing timestamp",
            preflight_distribution("4.2", ""),
            "",
            "build timestamp",
        ),
        (
            "ordinary dotted version",
            preflight_distribution("4.2", "4.2.0"),
            "",
            "supported build timestamp",
        ),
        (
            "different package version",
            preflight_distribution("4.2", "4.3.0.0.1.1341568473"),
            "",
            "supported build timestamp",
        ),
        (
            "invalid timestamp",
            preflight_distribution("4.2", "4.2.0.0.1.abcdefghij"),
            "",
            "invalid installer version",
        ),
        (
            "zero timestamp",
            preflight_distribution("4.2", "4.2.0.0.1.0000000000"),
            "",
            "invalid build timestamp",
        ),
        (
            "unrelated package",
            good.replace(
                "<bundle id=\"com.example.mac\"",
                "<bundle id=\"com.other.mac\"",
            ),
            "",
            "requested bundle",
        ),
        (
            "conflicting timestamps",
            good.replacen("1341568473", "1341568474", 1),
            "",
            "conflicting build timestamps",
        ),
        (
            "ambiguous products",
            good.replacen(
                "</installer-gui-script>",
                "<product id=\"com.other.mac\" version=\"4.2\"/></installer-gui-script>",
                1,
            ),
            "",
            "unique product",
        ),
        ("malformed XML", "<".to_owned(), "", "failed to parse"),
        (
            "another root element",
            "<pkg-info/>".to_owned(),
            "",
            "expected element type <installer-gui-script>",
        ),
    ];
    for (name, distribution, bundle, expected) in cases {
        let result = metadata(preflight_xar(distribution), bundle);
        if expected.is_empty() {
            let metadata = result.unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(metadata.display_version, "4.2", "{name}");
            assert_eq!(
                metadata.release_date,
                OffsetDateTime::from_unix_timestamp(1_341_568_473).expect("time"),
                "{name}"
            );
        } else {
            let err = result.err().map_or_else(String::new, |err| err.to_string());
            assert!(err.contains(expected), "{name}: {err}");
        }
    }
}

#[test]
fn rejects_corrupted_installer_metadata() {
    let mut data = preflight_xar(&preflight_distribution("4.2", "4.2.0.0.1.1341568473"));
    let last = data.len() - 1;
    data[last] ^= 1;
    assert_eq!(
        metadata(data, "com.example.mac")
            .err()
            .map(|e| e.to_string())
            .as_deref(),
        Some("macOS preflight Distribution checksum mismatch")
    );
}

#[test]
fn requires_one_distribution() {
    assert_eq!(
        metadata(test_archive("", b""), "")
            .err()
            .map(|e| e.to_string())
            .as_deref(),
        Some("macOS preflight package does not contain Distribution")
    );
}

#[test]
fn pads_short_display_versions() {
    let data = preflight_xar(&preflight_distribution("4", "4.0.0.0.1.1292967101"));
    assert_eq!(metadata(data, "").expect("metadata").display_version, "4");
}
