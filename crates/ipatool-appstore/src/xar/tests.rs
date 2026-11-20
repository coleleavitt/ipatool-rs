use std::fmt::Write as _;
use std::io::Write;

use sha1::{Digest, Sha1};

use super::*;

/// Go's `makeTestXARArchive` (`appstore_download_macos_test.go`): a SHA-1 TOC checksum at
/// heap offset 0, then `payload`.
pub(crate) fn test_archive(file_xml: &str, payload: &[u8]) -> Vec<u8> {
    let toc = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xar><toc><checksum style=\"sha1\"><offset>0</offset><size>20</size></checksum>{file_xml}</toc></xar>"
    );
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(toc.as_bytes()).expect("compress TOC");
    let compressed = encoder.finish().expect("compress TOC");
    let mut archive = Vec::new();
    archive.extend_from_slice(&HEADER_MAGIC.to_be_bytes());
    archive.extend_from_slice(&HEADER_SIZE.to_be_bytes());
    archive.extend_from_slice(&1u16.to_be_bytes());
    archive.extend_from_slice(&(compressed.len() as u64).to_be_bytes());
    archive.extend_from_slice(&(toc.len() as u64).to_be_bytes());
    archive.extend_from_slice(&1u32.to_be_bytes());
    archive.extend_from_slice(&compressed);
    archive.extend_from_slice(&Sha1::digest(&compressed));
    archive.extend_from_slice(payload);
    archive
}

/// Go's `makeTestXAR`: one `Payload` file.
pub(crate) fn test_xar(payload: &[u8], corrupt_file_checksum: bool) -> Vec<u8> {
    let sum = hex(&Sha1::digest(payload));
    let archived = if corrupt_file_checksum {
        "ff".repeat(20)
    } else {
        sum.clone()
    };
    let file = format!(
        "<file id=\"1\"><type>file</type><name>Payload</name><data><length>{len}</length><offset>20</offset><size>{len}</size><encoding style=\"application/octet-stream\"/><archived-checksum style=\"sha1\">{archived}</archived-checksum><extracted-checksum style=\"sha1\">{sum}</extracted-checksum></data></file>",
        len = payload.len()
    );
    test_archive(&file, payload)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, b| {
        let _ = write!(text, "{b:02x}");
        text
    })
}

fn reader(data: Vec<u8>) -> Reader<Vec<u8>> {
    let size = data.len() as u64;
    Reader::new(data, size).expect("valid XAR")
}

fn error(data: Vec<u8>) -> String {
    let size = data.len() as u64;
    Reader::new(data, size)
        .err()
        .expect("XAR error")
        .to_string()
}

#[test]
fn reads_files_and_verifies_checksums() {
    let archive = reader(test_xar(b"decrypted payload", false));
    let file = &archive.files[&1];
    assert_eq!(file.name, "Payload");
    assert_eq!(file.kind, FileType::File);
    assert!(archive.verify_checksum(file));
    assert_eq!(archive.read_file(file).expect("read"), b"decrypted payload");

    let corrupt = reader(test_xar(b"decrypted payload", true));
    assert!(!corrupt.verify_checksum(&corrupt.files[&1]));
}

#[test]
fn reports_go_header_errors() {
    assert_eq!(error(b"not xar".to_vec()), "EOF");
    let mut data = test_xar(b"x", false);
    data[0] = 0;
    assert_eq!(error(data), "xar: bad magic");
    let mut data = test_xar(b"x", false);
    data[7] = 2;
    assert_eq!(error(data), "xar: bad version");
    let mut data = test_xar(b"x", false);
    data[5] = 29;
    assert_eq!(error(data), "xar: bad header size");
    let mut data = test_xar(b"x", false);
    data[27] = 2;
    assert_eq!(error(data), "xar: header and toc checksum type mismatch");
    let mut data = test_xar(b"x", false);
    data[27] = 0;
    assert_eq!(error(data), "xar: unsupported checksum type");
}

#[test]
fn rejects_a_corrupt_toc_checksum() {
    let mut data = test_archive("", b"");
    let last = data.len() - 1;
    data[last] ^= 1;
    assert_eq!(error(data), "xar: checksum mismatch");
    // The stored checksum is cut short.
    let data = test_archive("", b"");
    assert_eq!(error(data[..data.len() - 1].to_vec()), "unexpected EOF");
}

#[test]
fn an_empty_toc_has_no_files() {
    assert!(reader(test_archive("", b"")).files.is_empty());
}

#[test]
fn nests_directory_names_and_skips_other_types() {
    let payload = b"<pkg-info/>";
    let sum = hex(&Sha1::digest(payload));
    let files = format!(
        "<file id=\"1\"><type>directory</type><name>app.pkg</name><file id=\"2\"><type>file</type><name>PackageInfo</name><mtime>2025-07-24T08:38:42Z</mtime><mode>0644</mode><data><length>11</length><offset>20</offset><size>11</size><encoding style=\"application/octet-stream\"/><archived-checksum style=\"SHA1\">{sum}</archived-checksum><extracted-checksum style=\"sha1\">{sum}</extracted-checksum></data></file></file><file id=\"bad\"><type>symlink</type><name>link</name></file>"
    );
    let archive = reader(test_archive(&files, payload));
    assert_eq!(archive.files.len(), 1);
    let file = &archive.files[&2];
    assert_eq!(file.name, "app.pkg/PackageInfo");
    assert_eq!(file.info.mtime, 1_753_346_322);
    assert_eq!(file.info.mode, 644);
    assert!(archive.verify_checksum(file));
}

#[test]
fn decodes_gzip_entries() {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"<pkg-info/>").expect("compress");
    let payload = encoder.finish().expect("compress");
    let sum = hex(&Sha1::digest(&payload));
    let extracted = hex(&Sha1::digest(b"<pkg-info/>"));
    let file = format!(
        "<file id=\"1\"><type>file</type><name>PackageInfo</name><data><length>{}</length><offset>20</offset><size>11</size><encoding style=\"application/x-gzip\"/><archived-checksum style=\"sha1\">{sum}</archived-checksum><extracted-checksum style=\"sha1\">{extracted}</extracted-checksum></data></file>",
        payload.len()
    );
    let archive = reader(test_archive(&file, &payload));
    assert_eq!(
        archive.read_file(&archive.files[&1]).expect("read"),
        b"<pkg-info/>"
    );
}

#[test]
fn rejects_malformed_entries_as_go_does() {
    let file = |body: &str| format!("<file id=\"1\"><type>file</type><name>x</name>{body}</file>");
    let data = "<data><length>0</length><offset>20</offset><size>0</size><encoding style=\"application/octet-stream\"/><archived-checksum style=\"sha1\">da39a3ee5e6b4b0d3255bfef95601890afd80709</archived-checksum><extracted-checksum style=\"sha1\">da39a3ee5e6b4b0d3255bfef95601890afd80709</extracted-checksum></data>";
    assert_eq!(error(test_archive(&file(""), b"")), "xar: file has no data");
    assert_eq!(
        error(test_archive(
            &file(&data.replace("style=\"sha1\">da", "style=\"crc\">da")),
            b""
        )),
        "xar: unsupported checksum type"
    );
    assert_eq!(
        error(test_archive(&file(&data.replacen("da39", "zz39", 1)), b"")),
        "encoding/hex: invalid byte: U+007A 'z'"
    );
    assert_eq!(
        error(test_archive(&file(&data.replacen("709<", "70<", 1)), b"")),
        "encoding/hex: odd length hex string"
    );
    assert_eq!(
        error(test_archive(&file(&format!("<uid>x</uid>{data}")), b"")),
        "strconv.ParseInt: parsing \"x\": invalid syntax"
    );
    assert!(
        error(test_archive(
            &file(&format!("<mtime>yesterday</mtime>{data}")),
            b""
        ))
        .starts_with("parsing time \"yesterday\"")
    );
    assert_eq!(
        error(test_archive(
            &file(data).replace("id=\"1\"", "id=\"-1\""),
            b""
        )),
        "strconv.ParseUint: parsing \"-1\": invalid syntax"
    );
    let archive = reader(test_archive(
        &file(&data.replace("octet-stream", "x-xz")),
        b"",
    ));
    assert_eq!(
        archive
            .read_file(&archive.files[&1])
            .err()
            .map(|e| e.to_string())
            .as_deref(),
        Some("xar: unsupported file encoding")
    );
}

#[test]
fn go_path_helpers() {
    use go_path::{base, clean, dir, join};
    assert_eq!(join("", "Distribution"), "Distribution");
    assert_eq!(join("a/", "../b"), "b");
    assert_eq!(join("", ""), "");
    assert_eq!(clean("/../a//b/."), "/a/b");
    assert_eq!(clean("../../a"), "../../a");
    assert_eq!(base("app.pkg/PackageInfo"), "PackageInfo");
    assert_eq!(base("a/"), "a");
    assert_eq!(base(""), ".");
    assert_eq!(dir("PackageInfo"), ".");
    assert_eq!(dir("app.pkg/PackageInfo"), "app.pkg");
    assert_eq!(join(&dir("PackageInfo"), "Payload"), "Payload");
}
