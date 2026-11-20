//! `appstore_zip_framing_test.go`, `appstore_zip_headers_test.go`,
//! `appstore_replicate_sinf_test.go` and `appstore_download_patches_test.go`.

use std::io::Write as _;

use flate2::Compression;
use flate2::write::DeflateEncoder;

use super::*;
use crate::download::apply_patches;
use crate::replicate_sinf::replicate_sinf;
use crate::zipfile::{Archive, DEFLATE, FileHeader, STORE, Writer, replicate, stored_extra};
use crate::{Account, DownloadItem, Sinf};

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
}

fn write(dir: &tempfile::TempDir, name: &str, data: &[u8]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, data).unwrap();
    path
}

fn replicate_bytes(path: &std::path::Path) -> Vec<u8> {
    let archive = Archive::open(path).unwrap();
    let mut writer = Writer::new(Vec::new());
    replicate(&archive, &mut writer).unwrap();
    writer.finish().unwrap()
}

#[test]
fn keeps_entry_data_with_streaming_framing() {
    for (name, method, source_flags) in [
        ("Payload/", STORE, 0),
        ("stored", STORE, 0),
        ("stored-descriptor", STORE, 8),
        ("deflated", DEFLATE, 0),
        ("deflated-descriptor", DEFLATE, 8),
    ] {
        let payload: &[u8] = if name.ends_with('/') {
            b""
        } else {
            b"streaming archive test payload"
        };
        let raw = if method == DEFLATE {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(payload).unwrap();
            encoder.finish().unwrap()
        } else {
            payload.to_vec()
        };
        let mut source = Writer::new(Vec::new());
        source
            .create_raw(
                FileHeader {
                    name: name.to_owned(),
                    method,
                    flags: source_flags,
                    crc32: crc32fast::hash(payload),
                    compressed_size64: raw.len() as u64,
                    uncompressed_size64: payload.len() as u64,
                    modified_date: 0x5821,
                    modified_time: 0x1234,
                    external_attrs: 0x81a4_0000,
                    ..FileHeader::default()
                },
                Vec::new(),
            )
            .unwrap();
        source.write_all(&raw).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "source.zip", &source.finish().unwrap());
        let output = replicate_bytes(&path);
        let result_path = write(&dir, "result.zip", &output);
        let result = Archive::open(&result_path).unwrap();
        let original = Archive::open(&path).unwrap();
        let (file, original) = (&result.files[0], &original.files[0]);
        assert_eq!(result.files.len(), 1);

        let offset = usize::try_from(result.data_offset(file).unwrap()).unwrap();
        let local = &output[..offset];
        let flags = u16_at(local, 6);
        assert_eq!(flags, file.flags, "{name}");
        let end = offset + usize::try_from(file.compressed_size64).unwrap();
        if method == DEFLATE {
            assert_eq!(flags & 8, 8);
            assert_eq!(&local[14..26], &[0; 12]);
            let descriptor = &output[end..end + 16];
            assert_eq!(u32_at(descriptor, 0), 0x0807_4b50);
            assert_eq!(u32_at(descriptor, 4), file.crc32);
            assert_eq!(u64::from(u32_at(descriptor, 8)), file.compressed_size64);
            assert_eq!(u64::from(u32_at(descriptor, 12)), file.uncompressed_size64);
        } else {
            assert_eq!(flags & 8, 0, "{name}");
            assert_eq!(u32_at(local, 14), file.crc32);
            assert_eq!(u64::from(u32_at(local, 18)), file.compressed_size64);
            assert_eq!(u64::from(u32_at(local, 22)), file.uncompressed_size64);
            assert_eq!(u32_at(&output, end), 0x0201_4b50);
        }
        let mut copied = Vec::new();
        std::io::Read::read_to_end(&mut result.open_raw(file).unwrap(), &mut copied).unwrap();
        assert_eq!(copied, raw);
        assert_eq!(file.name, original.name);
        assert_eq!(file.method, original.method);
        assert_eq!(file.modified_date, original.modified_date);
        assert_eq!(file.modified_time, original.modified_time);
        assert_eq!(file.external_attrs, original.external_attrs);
        assert_eq!(file.extra, original.extra);
        assert_eq!(result.read(file).unwrap(), payload);
    }
}

const UNIX_TIMESTAMP: [u8; 16] = [
    0x55, 0x58, 12, 0, 0xb1, 0x24, 0x51, 0x6a, 0xb1, 0x24, 0x51, 0x6a, 0xcb, 0, 0xc8, 0,
];
const LOCAL_TIMESTAMP: [u8; 13] = [
    0x55, 0x54, 9, 0, 3, 0xb2, 0x27, 0x51, 0x6a, 0xb2, 0x27, 0x51, 0x6a,
];
const CENTRAL_TIMESTAMP: [u8; 9] = [0x55, 0x54, 5, 0, 1, 0xb2, 0x27, 0x51, 0x6a];
const CODE_RESOURCES: &str = "Payload/Test.app/_CodeSignature/CodeResources";

/// Go's `timestampZIP`: distinct local and central extras on the first entry.
fn timestamp_zip(local: &[u8], central: &[u8]) -> Vec<u8> {
    let payload = b"signature";
    let mut writer = Writer::new(Vec::new());
    writer
        .create_raw(
            FileHeader {
                name: CODE_RESOURCES.to_owned(),
                method: STORE,
                modified_date: 0x5cea,
                modified_time: 0x874d,
                extra: local.to_vec(),
                crc32: crc32fast::hash(payload),
                compressed_size64: payload.len() as u64,
                uncompressed_size64: payload.len() as u64,
                ..FileHeader::default()
            },
            central.to_vec(),
        )
        .unwrap();
    writer.write_all(payload).unwrap();
    writer.create("Payload/Test.app/Info.plist").unwrap();
    writer
        .write_all(br#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleExecutable</key><string>Test</string></dict></plist>"#)
        .unwrap();
    writer.finish().unwrap()
}

/// Go's `checkTimestampZIP`.
fn check_timestamp_zip(path: &std::path::Path, local: &[u8], central: &[u8]) {
    let data = std::fs::read(path).unwrap();
    let name_len = usize::from(u16_at(&data, 26));
    let extra_len = usize::from(u16_at(&data, 28));
    assert_eq!(&data[30 + name_len..30 + name_len + extra_len], local);
    assert_eq!(u16_at(&data, 10), 0x874d);
    assert_eq!(u16_at(&data, 12), 0x5cea);
    let archive = Archive::open(path).unwrap();
    assert_eq!(archive.files[0].extra, central);
    assert_eq!(archive.read(&archive.files[0]).unwrap(), b"signature");
}

#[test]
fn keeps_independent_extras_through_metadata_and_sinf_patches() {
    for (local, central) in [
        (&UNIX_TIMESTAMP[..], &[][..]),
        (&CENTRAL_TIMESTAMP[..], &CENTRAL_TIMESTAMP[..]),
        (&LOCAL_TIMESTAMP[..], &[][..]),
        (&LOCAL_TIMESTAMP[..], &CENTRAL_TIMESTAMP[..]),
        (&[][..], &CENTRAL_TIMESTAMP[..]),
        (&[][..], &[][..]),
        (&[0xfe, 0xca, 3, 0, 1, 2, 3][..], &[][..]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let original = write(&dir, "original.ipa", &timestamp_zip(local, central));
        let patched = dir.path().join("patched.ipa");
        apply_patches(
            &DownloadItem::default(),
            &Account::default(),
            &original,
            &patched,
            &[],
        )
        .unwrap();
        check_timestamp_zip(&patched, local, central);
        let sinf = Sinf {
            data: b"sinf".to_vec(),
            ..Sinf::default()
        };
        replicate_sinf(&[sinf], &patched).unwrap();
        check_timestamp_zip(&patched, local, central);
    }
}

#[test]
fn reads_local_extras_of_reordered_and_prefixed_archives() {
    let data = timestamp_zip(&LOCAL_TIMESTAMP, &[]);
    let end = data.len() - 22;
    let start = usize::try_from(u32_at(&data, end + 16)).unwrap();
    let first =
        46 + usize::from(u16_at(&data, start + 28)) + usize::from(u16_at(&data, start + 30));
    let mut reordered = b"archive prefix".to_vec();
    reordered.extend_from_slice(&data[..start]);
    reordered.extend_from_slice(&data[start + first..end]);
    reordered.extend_from_slice(&data[start..start + first]);
    reordered.extend_from_slice(&data[end..]);
    let dir = tempfile::tempdir().unwrap();
    let archive = Archive::open(&write(&dir, "prefixed.zip", &reordered)).unwrap();
    assert_eq!(archive.files.len(), 2);
    for file in &archive.files {
        let extra = archive.local_extra(file).unwrap();
        if file.name == CODE_RESOURCES {
            assert_eq!(extra, LOCAL_TIMESTAMP);
            assert_eq!(archive.read(file).unwrap(), b"signature");
        }
    }
}

/// Go's `zip64TimestampZIP`: zip64 offsets and end records on a small archive.
fn zip64_timestamp_zip(local: &[u8]) -> Vec<u8> {
    let mut data = timestamp_zip(local, &[]);
    let end = data.len() - 22;
    let start = usize::try_from(u32_at(&data, end + 16)).unwrap();
    let name_len = usize::from(u16_at(&data, start + 28));
    let mut offset_extra = vec![0; 12];
    offset_extra[..2].copy_from_slice(&1_u16.to_le_bytes());
    offset_extra[2..4].copy_from_slice(&8_u16.to_le_bytes());
    data[start + 42..start + 46].copy_from_slice(&u32::MAX.to_le_bytes());
    data[start + 30..start + 32].copy_from_slice(&12_u16.to_le_bytes());
    let mut out = data[..start + 46 + name_len].to_vec();
    out.extend_from_slice(&offset_extra);
    out.extend_from_slice(&data[start + 46 + name_len..end]);
    let zip64_offset = out.len();
    let mut end64 = vec![0; 56];
    end64[..4].copy_from_slice(&0x0606_4b50_u32.to_le_bytes());
    end64[4..12].copy_from_slice(&44_u64.to_le_bytes());
    end64[24..32].copy_from_slice(&2_u64.to_le_bytes());
    end64[32..40].copy_from_slice(&2_u64.to_le_bytes());
    end64[40..48].copy_from_slice(&((zip64_offset - start) as u64).to_le_bytes());
    end64[48..56].copy_from_slice(&(start as u64).to_le_bytes());
    out.extend_from_slice(&end64);
    let mut locator = vec![0; 20];
    locator[..4].copy_from_slice(&0x0706_4b50_u32.to_le_bytes());
    locator[8..16].copy_from_slice(&(zip64_offset as u64).to_le_bytes());
    locator[16..20].copy_from_slice(&1_u32.to_le_bytes());
    out.extend_from_slice(&locator);
    let mut eocd = data[end..].to_vec();
    eocd[10..12].copy_from_slice(&0xffff_u16.to_le_bytes());
    eocd[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    eocd[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    out.extend_from_slice(&eocd);
    out
}

#[test]
fn reads_zip64_offsets_and_rewrites_without_stale_blocks() {
    let out = zip64_timestamp_zip(&LOCAL_TIMESTAMP);
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "source.zip", &out);
    let archive = Archive::open(&path).unwrap();
    assert_eq!(
        archive.local_extra(&archive.files[0]).unwrap(),
        LOCAL_TIMESTAMP
    );
    let result = write(&dir, "result.zip", &replicate_bytes(&path));
    check_timestamp_zip(&result, &LOCAL_TIMESTAMP, &[]);
}

#[test]
fn replaces_zip64_placeholder_sizes_of_stored_entries() {
    let mut extra = UNIX_TIMESTAMP.to_vec();
    let mut placeholder = vec![0; 20];
    placeholder[..2].copy_from_slice(&1_u16.to_le_bytes());
    placeholder[2..4].copy_from_slice(&16_u16.to_le_bytes());
    extra.extend_from_slice(&placeholder);
    let size = u64::from(u32::MAX) + 10;
    let header = FileHeader {
        method: STORE,
        compressed_size64: size,
        uncompressed_size64: size,
        ..FileHeader::default()
    };
    let result = stored_extra(&header, &extra);
    assert_eq!(&result[..UNIX_TIMESTAMP.len()], UNIX_TIMESTAMP);
    assert_eq!(result.len(), UNIX_TIMESTAMP.len() + 20);
    assert_eq!(u64_at(&result, UNIX_TIMESTAMP.len() + 4), size);
    assert_eq!(u64_at(&result, UNIX_TIMESTAMP.len() + 12), size);
}

#[test]
fn rejects_a_truncated_local_extra() {
    let mut data = timestamp_zip(&LOCAL_TIMESTAMP, &[]);
    data[28..30].copy_from_slice(&0xffff_u16.to_le_bytes());
    let dir = tempfile::tempdir().unwrap();
    let archive = Archive::open(&write(&dir, "truncated.zip", &data)).unwrap();
    assert!(archive.local_extra(&archive.files[0]).is_err());
}

/// An IPA with the given extra entries (name, content).
pub(crate) fn ipa(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = Writer::new(Vec::new());
    for (name, data) in entries {
        writer.create(name).unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap()
}

fn info_plist(executable: &str) -> Vec<u8> {
    let mut data = Vec::new();
    plist::to_writer_binary(
        &mut data,
        &plist::Value::Dictionary(dict(&[("CFBundleExecutable", s(executable))])),
    )
    .unwrap();
    data
}

fn manifest(paths: &[&str]) -> Vec<u8> {
    let mut data = Vec::new();
    let paths = plist::Value::Array(paths.iter().map(|path| s(path)).collect());
    plist::to_writer_xml(
        &mut data,
        &plist::Value::Dictionary(dict(&[("SinfPaths", paths)])),
    )
    .unwrap();
    data
}

fn sinf(data: &[u8]) -> Sinf {
    Sinf {
        id: 0,
        data: data.to_vec(),
        dp_info: Vec::new(),
    }
}

fn names_and_data(path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let archive = Archive::open(path).unwrap();
    archive
        .files
        .iter()
        .map(|file| (file.name.clone(), archive.read(file).unwrap()))
        .collect()
}

#[test]
fn replicates_sinfs_from_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let info = info_plist("Some");
    let manifest = manifest(&[
        "SC_Info/TestApp.sinf",
        "Frameworks/Ext.appex/SC_Info/Ext.sinf",
    ]);
    let path = write(
        &dir,
        "app.ipa",
        &ipa(&[
            ("Payload/Test.app/Info.plist", &info),
            ("Payload/Test.app/SC_Info/Manifest.plist", &manifest),
        ]),
    );
    replicate_sinf(&[sinf(b"one"), sinf(b"two")], &path).unwrap();
    let entries = names_and_data(&path);
    assert_eq!(entries.len(), 4);
    assert_eq!(
        entries[2],
        (
            "Payload/Test.app/SC_Info/TestApp.sinf".to_owned(),
            b"one".to_vec()
        )
    );
    assert_eq!(
        entries[3],
        (
            "Payload/Test.app/Frameworks/Ext.appex/SC_Info/Ext.sinf".to_owned(),
            b"two".to_vec()
        )
    );
    assert!(!dir.path().join("app.ipa.tmp").exists());
}

#[test]
fn replicates_a_sinf_from_info_plist_and_skips_watch_bundles() {
    let dir = tempfile::tempdir().unwrap();
    let info = info_plist("Executable");
    let path = write(
        &dir,
        "app.ipa",
        &ipa(&[
            ("Payload/Test.app/Watch/Watch.app/Info.plist", &info),
            ("Payload/Test.app/Info.plist", &info),
        ]),
    );
    replicate_sinf(&[sinf(b"one")], &path).unwrap();
    let entries = names_and_data(&path);
    assert_eq!(
        entries.last().unwrap(),
        &(
            "Payload/Test.app/SC_Info/Executable.sinf".to_owned(),
            b"one".to_vec()
        )
    );
}

#[test]
fn rewrites_without_sinfs_and_reports_go_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        &dir,
        "app.ipa",
        &ipa(&[("Payload/Test.app/Info.plist", &info_plist("X"))]),
    );
    replicate_sinf(&[], &path).unwrap();
    assert_eq!(names_and_data(&path).len(), 1);

    let invalid = write(&dir, "invalid.ipa", b"not a zip");
    assert_eq!(
        replicate_sinf(&[sinf(b"one")], &invalid)
            .unwrap_err()
            .to_string(),
        "failed to open zip reader"
    );
    let unnamed = write(&dir, "unnamed.ipa", &ipa(&[("Payload/readme", b"x")]));
    assert_eq!(
        replicate_sinf(&[sinf(b"one")], &unnamed)
            .unwrap_err()
            .to_string(),
        "failed to replicate sinf: failed to read bundle name: could not read bundle name"
    );
    let manifest = manifest(&["SC_Info/A.sinf"]);
    let mismatched = write(
        &dir,
        "mismatched.ipa",
        &ipa(&[
            ("Payload/Test.app/Info.plist", &info_plist("X")),
            ("Payload/Test.app/SC_Info/Manifest.plist", &manifest),
        ]),
    );
    assert_eq!(
        replicate_sinf(&[sinf(b"one"), sinf(b"two")], &mismatched)
            .unwrap_err()
            .to_string(),
        "failed to replicate sinf: failed to zip sinfs: slices have different lengths"
    );
}

#[test]
fn patches_add_artwork_and_metadata_with_the_account_email() {
    let dir = tempfile::tempdir().unwrap();
    let source = write(
        &dir,
        "app.ipa.tmp",
        &ipa(&[("Payload/Test.app/Info.plist", &info_plist("X"))]),
    );
    let destination = dir.path().join("app.ipa");
    let item = DownloadItem {
        metadata: dict(&[
            ("bundleShortVersionString", s("1.0")),
            ("itemId", plist::Value::Integer(42.into())),
        ]),
        ..DownloadItem::default()
    };
    let account = Account {
        email: "user@example.com".to_owned(),
        ..Account::default()
    };
    apply_patches(&item, &account, &source, &destination, b"artwork").unwrap();
    let entries = names_and_data(&destination);
    let names: Vec<_> = entries.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Payload/Test.app/Info.plist",
            "iTunesArtwork",
            "iTunesMetadata.plist"
        ]
    );
    assert_eq!(entries[1].1, b"artwork");
    assert!(entries[2].1.starts_with(b"bplist00"));
    let metadata: plist::Dictionary = plist::from_bytes(&entries[2].1).unwrap();
    assert_eq!(
        metadata.get("apple-id").and_then(plist::Value::as_string),
        Some("user@example.com")
    );
    assert_eq!(
        metadata.get("userName").and_then(plist::Value::as_string),
        Some("user@example.com")
    );
    assert_eq!(
        metadata
            .get("bundleShortVersionString")
            .and_then(plist::Value::as_string),
        Some("1.0")
    );
    let keys: Vec<_> = metadata.keys().cloned().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);

    apply_patches(&item, &account, &source, &destination, &[]).unwrap();
    let names: Vec<_> = names_and_data(&destination)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names,
        ["Payload/Test.app/Info.plist", "iTunesMetadata.plist"]
    );
}
