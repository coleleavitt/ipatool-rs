//! Extraction from the macOS restore image. Port of Go `assets/remote.go` and its readers.

mod aea;
mod apfs;
mod dyld;
mod dyld_symbols;
mod http_range;
mod kernel;
mod lzbitmap;
mod lzvn;
mod macho;

use std::io::{Read, Seek, SeekFrom};

use reqwest::blocking::Client;

use super::{Bundle, MACOS_BUILD, MACOS_RESTORE_URL, MACOS_VERSION, REQUIRED_FILES};
use crate::error::{Context, Error, Result};

const APPLE_MEDIA_SERVICES_IMAGE: &str =
    "/System/Library/PrivateFrameworks/AppleMediaServices.framework/Versions/A/AppleMediaServices";
const FILE_SYSTEM_IMAGE_NAME: &str = "043-70867-640.dmg.aea";
const SYSTEM_OS_IMAGE_NAME: &str = "043-70701-651.dmg.aea";
const FILE_SYSTEM_ARCHIVE_SIZE: u64 = 11_274_289_152;
const FILE_SYSTEM_IMAGE_SIZE: u64 = 14_189_330_432;
const SYSTEM_OS_ARCHIVE_SIZE: u64 = 2_415_919_104;
const SYSTEM_OS_IMAGE_SIZE: u64 = 2_879_389_696;
const ARM64_CACHE_PATH: &str = "/System/Library/dyld/dyld_shared_cache_arm64e";

const FRAMEWORK_IMAGE_PATHS: [&str; 5] = [
    "/System/Library/PrivateFrameworks/CoreADI.framework/Versions/A/adid",
    "/System/Library/PrivateFrameworks/CommerceKit.framework/Versions/A/Resources/commerce",
    "/System/Library/PrivateFrameworks/CoreFP.framework/Versions/A/CoreFP",
    "/System/Library/PrivateFrameworks/CoreFP.framework/Versions/A/Resources/CoreFP.icxs",
    "/System/Library/PrivateFrameworks/CoreFP.framework/Versions/A/fairplayd",
];

/// Random access that fills the whole buffer or fails.
pub(super) trait ReadAt: Sync {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()>;
}

impl ReadAt for [u8] {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()> {
        let source = usize::try_from(offset)
            .ok()
            .and_then(|start| self.get(start..start.checked_add(data.len())?))
            .ok_or_else(|| Error::msg("EOF"))?;
        data.copy_from_slice(source);
        Ok(())
    }
}

/// A window of another reader, like Go's `io.SectionReader`.
pub(super) struct Section<'a> {
    source: &'a dyn ReadAt,
    offset: u64,
    size: u64,
}

impl<'a> Section<'a> {
    pub(super) fn new(source: &'a dyn ReadAt, offset: u64, size: u64) -> Self {
        Self {
            source,
            offset,
            size,
        }
    }
}

impl ReadAt for Section<'_> {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()> {
        if offset > self.size || data.len() as u64 > self.size - offset {
            return Err(Error::msg("EOF"));
        }
        self.source.read_at(data, self.offset + offset)
    }
}

/// `Read + Seek` over a `ReadAt`, for the zip reader.
pub(super) struct Cursor<'a> {
    source: &'a dyn ReadAt,
    size: u64,
    position: u64,
}

impl Read for Cursor<'_> {
    fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.size.saturating_sub(self.position);
        let count = usize::try_from(remaining.min(data.len() as u64)).unwrap_or(data.len());
        self.source
            .read_at(&mut data[..count], self.position)
            .map_err(std::io::Error::other)?;
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for Cursor<'_> {
    fn seek(&mut self, target: SeekFrom) -> std::io::Result<u64> {
        let position = match target {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.size.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        };
        self.position = position.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "negative position")
        })?;
        Ok(self.position)
    }
}

pub(super) type Archive<'a> = zip::ZipArchive<Cursor<'a>>;

/// A member that matched its pinned method and sizes.
pub(super) struct ZipEntry {
    index: usize,
    data_offset: u64,
    compressed_size: u64,
}

/// Returns the frameworks as stored in the restore image (universal binaries intact).
pub(super) fn resolve_remote_image() -> Result<Bundle> {
    let client = Client::new();
    let remote = http_range::RemoteFile::open(client.clone(), MACOS_RESTORE_URL)
        .with_context(|| format!("resolve macOS {MACOS_VERSION} ({MACOS_BUILD}) frameworks"))?;
    resolve_image(&client, &remote, remote.size())
}

fn resolve_image(client: &Client, remote: &dyn ReadAt, size: u64) -> Result<Bundle> {
    let mut archive = zip::ZipArchive::new(Cursor {
        source: remote,
        size,
        position: 0,
    })
    .with_context(|| format!("read macOS {MACOS_VERSION} ({MACOS_BUILD}) restore image"))?;
    let file_system_entry = pinned_zip_entry(
        &mut archive,
        FILE_SYSTEM_IMAGE_NAME,
        FILE_SYSTEM_ARCHIVE_SIZE,
        FILE_SYSTEM_ARCHIVE_SIZE,
    )?;
    let system_os_entry = pinned_zip_entry(
        &mut archive,
        SYSTEM_OS_IMAGE_NAME,
        SYSTEM_OS_ARCHIVE_SIZE,
        SYSTEM_OS_ARCHIVE_SIZE,
    )?;
    let mut files = extract_framework_files(client, remote, &file_system_entry)?;
    let apple_media_services = extract_apple_media_services(client, remote, &system_os_entry)
        .with_context(|| {
            format!("extract AppleMediaServices from macOS {MACOS_VERSION} ({MACOS_BUILD})")
        })?;
    files.insert("AppleMediaServices".to_owned(), apple_media_services);
    files.insert(
        "FairPlayIOKit".to_owned(),
        kernel::extract_fair_play_kext(&mut archive)?,
    );

    let mut bundle = Bundle::default();
    for spec in &REQUIRED_FILES {
        *bundle.file_mut(spec.name) = files
            .remove(spec.name)
            .ok_or_else(|| Error::msg(format!("read extracted {}: file not found", spec.name)))?;
    }
    Ok(bundle)
}

fn open_encrypted<'a>(
    client: &Client,
    encrypted: &'a Section<'a>,
    entry: &ZipEntry,
) -> Result<aea::AeaReader<'a>> {
    aea::AeaReader::open(client, encrypted, entry.compressed_size)
}

fn extract_apple_media_services(
    client: &Client,
    remote: &dyn ReadAt,
    entry: &ZipEntry,
) -> Result<Vec<u8>> {
    let encrypted = Section::new(remote, entry.data_offset, entry.compressed_size);
    let image =
        open_encrypted(client, &encrypted, entry).context("open encrypted SystemOS cryptex")?;
    if image.size() != SYSTEM_OS_IMAGE_SIZE {
        return Err(Error::msg(format!(
            "SystemOS cryptex has size {}, expected {SYSTEM_OS_IMAGE_SIZE}",
            image.size()
        )));
    }
    let extractor = apfs::Extractor::open(&image, image.size())?;
    let opener = |suffix: &str| -> Result<apfs::File<'_>> {
        extractor
            .open_file(&format!("{ARM64_CACHE_PATH}{suffix}"))
            .with_context(|| format!("open arm64e shared-cache member {suffix:?}"))
    };
    dyld::extract_dyld_image_from_files(&opener, APPLE_MEDIA_SERVICES_IMAGE)
        .context("export AppleMediaServices from arm64e shared cache")
}

pub(super) fn pinned_zip_entry(
    archive: &mut Archive<'_>,
    name: &str,
    compressed_size: u64,
    uncompressed_size: u64,
) -> Result<ZipEntry> {
    let expected_method = if compressed_size == uncompressed_size {
        zip::CompressionMethod::Stored
    } else {
        zip::CompressionMethod::Deflated
    };
    let Some(index) = archive.index_for_name(name) else {
        return Err(Error::msg(format!(
            "macOS {MACOS_VERSION} ({MACOS_BUILD}) restore image does not contain {name}"
        )));
    };
    let entry = archive
        .by_index_raw(index)
        .with_context(|| format!("macOS {MACOS_VERSION} ({MACOS_BUILD}) restore member {name}"))?;
    if entry.compression() != expected_method
        || entry.compressed_size() != compressed_size
        || entry.size() != uncompressed_size
    {
        return Err(Error::msg(format!(
            "macOS {MACOS_VERSION} ({MACOS_BUILD}) restore member {name} does not match the pinned image"
        )));
    }
    let data_offset = entry
        .data_start()
        .ok_or_else(|| Error::msg(format!("locate restore member {name}")))?;
    Ok(ZipEntry {
        index,
        data_offset,
        compressed_size,
    })
}

fn extract_framework_files(
    client: &Client,
    remote: &dyn ReadAt,
    entry: &ZipEntry,
) -> Result<std::collections::HashMap<String, Vec<u8>>> {
    let encrypted = Section::new(remote, entry.data_offset, entry.compressed_size);
    let image =
        open_encrypted(client, &encrypted, entry).context("open encrypted macOS filesystem")?;
    if image.size() != FILE_SYSTEM_IMAGE_SIZE {
        return Err(Error::msg(format!(
            "macOS filesystem has size {}, expected {FILE_SYSTEM_IMAGE_SIZE}",
            image.size()
        )));
    }
    apfs::extract_apfs_files(&image, image.size(), &FRAMEWORK_IMAGE_PATHS)
        .with_context(|| format!("extract frameworks from macOS {MACOS_VERSION} ({MACOS_BUILD})"))
}

/// Decodes an LZFSE stream that must produce exactly `expected` bytes.
pub(super) fn lzfse_decode(compressed: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut output = vec![0u8; expected + 1];
    match lzfse::decode_buffer(compressed, &mut output) {
        Ok(count) if count == expected => {
            output.truncate(count);
            Ok(output)
        }
        Ok(0) => Err(Error::msg("invalid LZFSE stream")),
        Ok(count) => Err(Error::msg(format!(
            "LZFSE stream produced {count} bytes, expected {expected}"
        ))),
        Err(_) => Err(Error::msg(format!(
            "LZFSE stream produced more than {expected} bytes"
        ))),
    }
}

pub(super) fn le16(data: &[u8]) -> u16 {
    u16::from_le_bytes([data[0], data[1]])
}

pub(super) fn le32(data: &[u8]) -> u32 {
    u32::from_le_bytes([data[0], data[1], data[2], data[3]])
}

pub(super) fn le32_words(data: &[u8]) -> Vec<u32> {
    data.as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_le_bytes(*word))
        .collect()
}

pub(super) fn le64(data: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[..8]);
    u64::from_le_bytes(bytes)
}
