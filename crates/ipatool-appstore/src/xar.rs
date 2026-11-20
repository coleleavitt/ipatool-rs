//! XAR archive reader: a port of `github.com/blacktop/go-macho/pkg/xar` (v1.1.282), the
//! reader Go's macOS `.pkg` validation and version metadata use.
//!
//! Signature verification is not ported: Go stores its result in `SignatureError`, and
//! ipatool never reads it.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read};
use std::path::Path;

use md5::Md5;
use sha1::{Digest, Sha1};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const HEADER_MAGIC: u32 = 0x7861_7221;
const HEADER_VERSION: u16 = 1;
const HEADER_SIZE: u16 = 28;
const CHECKSUM_KIND_SHA1: u32 = 1;
const CHECKSUM_KIND_MD5: u32 = 2;
const COPY_CHUNK: usize = 1 << 20;

/// The package's errors, with go-macho's and the Go standard library's texts.
#[derive(Debug)]
pub(crate) enum Error {
    BadMagic,
    BadVersion,
    BadHeaderSize,
    NoTocChecksum,
    ChecksumUnsupported,
    ChecksumTypeMismatch,
    ChecksumMismatch,
    FileNoData,
    FileEncodingUnsupported,
    /// `io.EOF`.
    Eof,
    /// `io.ErrUnexpectedEOF`.
    UnexpectedEof,
    Io(io::Error),
    /// TOC, number, time and hex decoding failures.
    Decode(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BadMagic => "xar: bad magic",
            Self::BadVersion => "xar: bad version",
            Self::BadHeaderSize => "xar: bad header size",
            Self::NoTocChecksum => "xar: no TOC checksum info in TOC",
            Self::ChecksumUnsupported => "xar: unsupported checksum type",
            Self::ChecksumTypeMismatch => "xar: header and toc checksum type mismatch",
            Self::ChecksumMismatch => "xar: checksum mismatch",
            Self::FileNoData => "xar: file has no data",
            Self::FileEncodingUnsupported => "xar: unsupported file encoding",
            Self::Eof => "EOF",
            Self::UnexpectedEof => "unexpected EOF",
            Self::Io(err) => return err.fmt(f),
            Self::Decode(message) => message,
        })
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

/// Go's `io.ReaderAt`: a short read is `io.EOF`.
pub(crate) trait ReadAt {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
}

impl ReadAt for [u8] {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.len());
        let available = &self[start..];
        let count = available.len().min(buf.len());
        buf[..count].copy_from_slice(&available[..count]);
        Ok(count)
    }
}

impl ReadAt for Vec<u8> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.as_slice().read_at(buf, offset)
    }
}

impl ReadAt for std::fs::File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            #[cfg(unix)]
            let count = std::os::unix::fs::FileExt::read_at(
                self,
                &mut buf[filled..],
                offset + filled as u64,
            );
            #[cfg(windows)]
            let count = std::os::windows::fs::FileExt::seek_read(
                self,
                &mut buf[filled..],
                offset + filled as u64,
            );
            match count {
                Ok(0) => break,
                Ok(count) => filled += count,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(filled)
    }
}

/// `ReaderAt.ReadAt` into the whole buffer: a short read is `io.EOF`.
fn read_exact_at(source: &(impl ReadAt + ?Sized), buf: &mut [u8], offset: u64) -> Result<()> {
    let count = source.read_at(buf, offset).map_err(Error::Io)?;
    if count < buf.len() {
        return Err(Error::Eof);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileType {
    File,
    Directory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChecksumKind {
    Sha1,
    Md5,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileChecksum {
    pub kind: ChecksumKind,
    pub sum: Vec<u8>,
}

/// Go's `FileInfo`; times are Unix seconds, zero when absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub device_no: u64,
    pub mode: u32,
    pub inode: u64,
    pub uid: i64,
    pub user: String,
    pub gid: i64,
    pub group: String,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
}

/// A file or directory entry. Directories are not listed in [`Reader::files`].
#[derive(Clone, Debug)]
pub(crate) struct File {
    pub kind: FileType,
    pub info: FileInfo,
    pub name: String,
    pub encoding_mimetype: String,
    pub compressed_checksum: Option<FileChecksum>,
    offset: i64,
    length: i64,
}

/// Go's `xar.Reader`. Go keeps the files in a map, so callers must not depend on order;
/// here they are sorted by ID.
pub(crate) struct Reader<R: ?Sized> {
    pub files: BTreeMap<u64, File>,
    heap_offset: u64,
    size: u64,
    source: R,
}

impl Reader<std::fs::File> {
    /// Go's `xar.Open`.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(Error::Io)?;
        let size = file.metadata().map_err(Error::Io)?.len();
        Self::new(file, size)
    }
}

impl<R: ReadAt> Reader<R> {
    /// Go's `xar.NewReader`.
    pub(crate) fn new(source: R, size: u64) -> Result<Self> {
        let mut header = [0; HEADER_SIZE as usize];
        read_exact_at(&source, &mut header, 0)?;
        let be16 = |at: usize| u16::from_be_bytes([header[at], header[at + 1]]);
        let be32 = |at: usize| {
            u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
        };
        let be64 = |at: usize| (u64::from(be32(at)) << 32) | u64::from(be32(at + 4));
        if be32(0) != HEADER_MAGIC {
            return Err(Error::BadMagic);
        }
        if be16(6) != HEADER_VERSION {
            return Err(Error::BadVersion);
        }
        if be16(4) != HEADER_SIZE {
            return Err(Error::BadHeaderSize);
        }
        let toc_length = be64(8);
        let checksum_kind = be32(24);

        // Go allocates the whole TOC before reading; a length past the end is the same EOF.
        if toc_length > size.saturating_sub(u64::from(HEADER_SIZE)) {
            return Err(Error::Eof);
        }
        let mut compressed_toc = vec![0; usize::try_from(toc_length).map_err(|_| Error::Eof)?];
        read_exact_at(&source, &mut compressed_toc, u64::from(HEADER_SIZE))?;
        let mut toc = Vec::new();
        flate2::read::ZlibDecoder::new(compressed_toc.as_slice())
            .read_to_end(&mut toc)
            .map_err(|err| zlib_error(&err))?;
        let toc =
            String::from_utf8(toc).map_err(|_| Error::Decode("xml: invalid UTF-8".to_owned()))?;
        let root = Toc::parse(&toc)?;

        let heap_offset = u64::from(HEADER_SIZE) + toc_length;
        let checksum = root.checksum.ok_or(Error::NoTocChecksum)?;
        let stored = read_section_full(&source, heap_offset, checksum.offset, checksum.size)?;
        let calculated = match checksum_kind {
            CHECKSUM_KIND_SHA1 if checksum.style == "sha1" => {
                Sha1::digest(&compressed_toc).to_vec()
            }
            CHECKSUM_KIND_MD5 if checksum.style == "md5" => Md5::digest(&compressed_toc).to_vec(),
            CHECKSUM_KIND_SHA1 | CHECKSUM_KIND_MD5 => return Err(Error::ChecksumTypeMismatch),
            _ => return Err(Error::ChecksumUnsupported),
        };
        if calculated != stored {
            return Err(Error::ChecksumMismatch);
        }

        let mut files = BTreeMap::new();
        for entry in &root.files {
            read_file_tree(entry, "", &mut files)?;
        }
        Ok(Self {
            files,
            heap_offset,
            size,
            source,
        })
    }

    /// Go's `File.VerifyChecksum`: the archived bytes match the archived checksum.
    pub(crate) fn verify_checksum(&self, file: &File) -> bool {
        if file.kind != FileType::File {
            return true;
        }
        let Some(checksum) = &file.compressed_checksum else {
            return false;
        };
        let sum = match checksum.kind {
            ChecksumKind::Sha1 => self.hash_section::<Sha1>(file),
            ChecksumKind::Md5 => self.hash_section::<Md5>(file),
        };
        sum == checksum.sum
    }

    /// Go's `File.Open` read to the end: the extracted contents.
    pub(crate) fn read_file(&self, file: &File) -> Result<Vec<u8>> {
        let mut raw = Vec::new();
        self.copy_section(file, |chunk| raw.extend_from_slice(chunk))?;
        let mut data = Vec::new();
        match file.encoding_mimetype.as_str() {
            "application/octet-stream" => return Ok(raw),
            "application/x-gzip" => {
                flate2::read::ZlibDecoder::new(raw.as_slice())
                    .read_to_end(&mut data)
                    .map_err(|err| zlib_error(&err))?;
            }
            "application/x-bzip2" => {
                bzip2::read::BzDecoder::new(raw.as_slice())
                    .read_to_end(&mut data)
                    .map_err(|err| Error::Decode(format!("bzip2 data invalid: {err}")))?;
            }
            _ => return Err(Error::FileEncodingUnsupported),
        }
        Ok(data)
    }

    fn hash_section<D: Digest>(&self, file: &File) -> Vec<u8> {
        let mut hasher = D::new();
        // Go ignores the copy error and compares what it read.
        let _ = self.copy_section(file, |chunk| hasher.update(chunk));
        hasher.finalize().to_vec()
    }

    /// Go's `io.NewSectionReader(heap, offset, length)` read to the end; the heap ends at
    /// the archive size, and reads outside it see `io.EOF`.
    fn copy_section(&self, file: &File, mut sink: impl FnMut(&[u8])) -> Result<()> {
        let heap_size = self.size.saturating_sub(self.heap_offset);
        let (Ok(offset), Ok(length)) = (u64::try_from(file.offset), u64::try_from(file.length))
        else {
            return Ok(());
        };
        let end = offset.saturating_add(length).min(heap_size);
        let mut position = offset;
        let mut buffer = vec![0; COPY_CHUNK];
        while position < end {
            let want = usize::try_from(end - position).map_or(COPY_CHUNK, |n| n.min(COPY_CHUNK));
            let count = self
                .source
                .read_at(&mut buffer[..want], self.heap_offset + position)
                .map_err(Error::Io)?;
            if count == 0 {
                break;
            }
            sink(&buffer[..count]);
            position += count as u64;
        }
        Ok(())
    }
}

/// `io.ReadFull` of `io.NewSectionReader(source, heap + offset, size)`.
fn read_section_full(
    source: &impl ReadAt,
    heap_offset: u64,
    offset: i64,
    size: i64,
) -> Result<Vec<u8>> {
    let size = usize::try_from(size)
        .map_err(|_| Error::Decode("xar: negative checksum size".to_owned()))?;
    let mut buffer = vec![0; size];
    if size == 0 {
        return Ok(buffer);
    }
    let start = u64::try_from(offset)
        .ok()
        .map(|offset| heap_offset + offset);
    let count = match start {
        Some(start) => source.read_at(&mut buffer, start).map_err(Error::Io)?,
        None => 0,
    };
    match count {
        0 => Err(Error::Eof),
        count if count < size => Err(Error::UnexpectedEof),
        _ => Ok(buffer),
    }
}

fn zlib_error(err: &io::Error) -> Error {
    if err.kind() == io::ErrorKind::UnexpectedEof {
        return Error::UnexpectedEof;
    }
    Error::Decode(format!("zlib: {err}"))
}

fn read_file_tree(entry: &TocFile, directory: &str, files: &mut BTreeMap<u64, File>) -> Result<()> {
    let kind = match entry.kind.as_str() {
        "file" => FileType::File,
        "directory" => FileType::Directory,
        _ => return Ok(()),
    };
    let id = entry
        .id
        .parse::<u64>()
        .ok()
        .filter(|_| is_go_unsigned(&entry.id))
        .ok_or_else(|| {
            Error::Decode(format!(
                "strconv.ParseUint: parsing {}: invalid syntax",
                go_quote(&entry.id)
            ))
        })?;
    let name = go_path::join(directory, &entry.name);
    let info = FileInfo {
        device_no: entry.device_no,
        mode: entry.mode,
        inode: entry.inode,
        uid: entry.uid,
        user: entry.user.clone(),
        gid: entry.gid,
        group: entry.group.clone(),
        atime: unix_time(&entry.atime)?,
        mtime: unix_time(&entry.mtime)?,
        ctime: unix_time(&entry.ctime)?,
    };
    if kind == FileType::Directory {
        for child in &entry.files {
            read_file_tree(child, &name, files)?;
        }
        return Ok(());
    }
    let data = entry.data.as_ref().ok_or(Error::FileNoData)?;
    let compressed_checksum = file_checksum(&data.archived_checksum)?;
    // Go keeps the extracted checksum and size; nothing reads them, but a bad one still
    // fails the archive.
    file_checksum(&data.extracted_checksum)?;
    let file = File {
        kind,
        info,
        name,
        encoding_mimetype: data.encoding.clone(),
        compressed_checksum: Some(compressed_checksum),
        offset: data.offset,
        length: data.length,
    };
    files.insert(id, file);
    Ok(())
}

/// `strconv.ParseUint(s, 10, 0)` accepts only ASCII digits (Rust also takes a leading `+`).
fn is_go_unsigned(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Go's `fileChecksumFromXml`: the digest is decoded before the style is checked.
fn file_checksum(checksum: &TocChecksum) -> Result<FileChecksum> {
    let sum = decode_hex(&checksum.digest)?;
    let kind = match checksum.style.to_uppercase().as_str() {
        "MD5" => ChecksumKind::Md5,
        "SHA1" => ChecksumKind::Sha1,
        _ => return Err(Error::ChecksumUnsupported),
    };
    Ok(FileChecksum { kind, sum })
}

/// `encoding/hex.DecodeString`, with its error texts.
fn decode_hex(text: &str) -> Result<Vec<u8>> {
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let (pairs, remainder) = bytes.as_chunks::<2>();
    for pair in pairs {
        let high = nibble(pair[0]).ok_or_else(|| invalid_hex_byte(pair[0]))?;
        let low = nibble(pair[1]).ok_or_else(|| invalid_hex_byte(pair[1]))?;
        out.push(high << 4 | low);
    }
    if let [last] = remainder {
        // Go reports an invalid trailing byte before the odd length.
        if nibble(*last).is_none() {
            return Err(invalid_hex_byte(*last));
        }
        return Err(Error::Decode(
            "encoding/hex: odd length hex string".to_owned(),
        ));
    }
    Ok(out)
}

/// `hex.InvalidByteError`, printed with `%#U`.
fn invalid_hex_byte(byte: u8) -> Error {
    let rune = char::from(byte);
    let printable = !rune.is_control() && rune != '\u{ad}';
    Error::Decode(if printable {
        format!(
            "encoding/hex: invalid byte: U+{:04X} '{rune}'",
            u32::from(byte)
        )
    } else {
        format!("encoding/hex: invalid byte: U+{:04X}", u32::from(byte))
    })
}

fn go_quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\""))
}

/// `time.Parse(time.RFC3339, value).Unix()`, zero for an empty value.
fn unix_time(value: &str) -> Result<i64> {
    if value.is_empty() {
        return Ok(0);
    }
    OffsetDateTime::parse(value, &Rfc3339)
        .map(OffsetDateTime::unix_timestamp)
        .map_err(|err| {
            Error::Decode(format!(
                "parsing time {} as \"2006-01-02T15:04:05Z07:00\": {err}",
                go_quote(value)
            ))
        })
}

#[derive(Debug, Default)]
struct Toc {
    checksum: Option<TocHeapRange>,
    files: Vec<TocFile>,
}

#[derive(Debug, Default)]
struct TocHeapRange {
    style: String,
    offset: i64,
    size: i64,
}

#[derive(Debug, Default)]
struct TocFile {
    id: String,
    kind: String,
    name: String,
    ctime: String,
    mtime: String,
    atime: String,
    group: String,
    gid: i64,
    user: String,
    uid: i64,
    mode: u32,
    device_no: u64,
    inode: u64,
    data: Option<TocData>,
    files: Vec<TocFile>,
}

#[derive(Debug, Default)]
struct TocData {
    length: i64,
    offset: i64,
    encoding: String,
    archived_checksum: TocChecksum,
    extracted_checksum: TocChecksum,
}

#[derive(Debug, Default)]
struct TocChecksum {
    style: String,
    digest: String,
}

impl Toc {
    /// Go's `xml.Decoder.Decode` into `xmlXar`. Like `encoding/xml`, a repeated scalar
    /// element overwrites the earlier value, a repeated struct element merges into it and
    /// list elements append. Every numeric field Go declares is parsed, so a malformed
    /// number fails the archive as in Go, even in fields ipatool does not use.
    fn parse(text: &str) -> Result<Self> {
        let document = roxmltree::Document::parse_with_options(
            text,
            roxmltree::ParsingOptions {
                allow_dtd: true,
                ..roxmltree::ParsingOptions::default()
            },
        )
        .map_err(|err| Error::Decode(format!("XML syntax error: {err}")))?;
        let root = document.root_element();
        if root.tag_name().name() != "xar" {
            return Err(Error::Decode(format!(
                "expected element type <xar> but have <{}>",
                root.tag_name().name()
            )));
        }
        let mut toc = Self::default();
        for child in root.children().filter(roxmltree::Node::is_element) {
            match child.tag_name().name() {
                "toc" => toc.merge(child)?,
                "subdoc" => {
                    for field in child.children().filter(roxmltree::Node::is_element) {
                        if field.tag_name().name() == "hide-symbols" {
                            go_int::<i64>(field)?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(toc)
    }

    fn merge(&mut self, node: roxmltree::Node<'_, '_>) -> Result<()> {
        for child in node.children().filter(roxmltree::Node::is_element) {
            match child.tag_name().name() {
                "checksum" => {
                    let checksum = self.checksum.get_or_insert_with(TocHeapRange::default);
                    checksum.merge(child)?;
                }
                "signature" => TocHeapRange::default().merge(child)?,
                "signature-creation-time" => {
                    go_int::<i64>(child)?;
                }
                "file" => {
                    let mut file = TocFile::default();
                    file.merge(child)?;
                    self.files.push(file);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl TocHeapRange {
    fn merge(&mut self, node: roxmltree::Node<'_, '_>) -> Result<()> {
        if let Some(style) = node.attribute("style") {
            style.clone_into(&mut self.style);
        }
        for child in node.children().filter(roxmltree::Node::is_element) {
            match child.tag_name().name() {
                "offset" => self.offset = go_int(child)?,
                "size" => self.size = go_int(child)?,
                _ => {}
            }
        }
        Ok(())
    }
}

impl TocFile {
    fn merge(&mut self, node: roxmltree::Node<'_, '_>) -> Result<()> {
        if let Some(id) = node.attribute("id") {
            id.clone_into(&mut self.id);
        }
        for child in node.children().filter(roxmltree::Node::is_element) {
            match child.tag_name().name() {
                "ctime" => self.ctime = text(child),
                "mtime" => self.mtime = text(child),
                "atime" => self.atime = text(child),
                "group" => self.group = text(child),
                "gid" => self.gid = go_int(child)?,
                "user" => self.user = text(child),
                "uid" => self.uid = go_int(child)?,
                "mode" => self.mode = go_int(child)?,
                "deviceno" => self.device_no = go_int(child)?,
                "inode" => self.inode = go_int(child)?,
                "type" => self.kind = text(child),
                "name" => self.name = text(child),
                "FinderCreateTime" => {
                    for field in child.children().filter(roxmltree::Node::is_element) {
                        if field.tag_name().name() == "nanoseconds" {
                            go_int::<i64>(field)?;
                        }
                    }
                }
                "data" => self
                    .data
                    .get_or_insert_with(TocData::default)
                    .merge(child)?,
                "file" => {
                    let mut file = Self::default();
                    file.merge(child)?;
                    self.files.push(file);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl TocData {
    fn merge(&mut self, node: roxmltree::Node<'_, '_>) -> Result<()> {
        for child in node.children().filter(roxmltree::Node::is_element) {
            match child.tag_name().name() {
                "length" => self.length = go_int(child)?,
                "offset" => self.offset = go_int(child)?,
                "size" => {
                    go_int::<i64>(child)?;
                }
                "encoding" => {
                    if let Some(style) = child.attribute("style") {
                        style.clone_into(&mut self.encoding);
                    }
                }
                "archived-checksum" => self.archived_checksum.merge(child),
                "extracted-checksum" => self.extracted_checksum.merge(child),
                _ => {}
            }
        }
        Ok(())
    }
}

impl TocChecksum {
    fn merge(&mut self, node: roxmltree::Node<'_, '_>) {
        if let Some(style) = node.attribute("style") {
            style.clone_into(&mut self.style);
        }
        self.digest = text(node);
    }
}

/// An element's own character data, as `encoding/xml` collects it for a scalar field
/// (text inside child elements is skipped).
pub(crate) fn text(node: roxmltree::Node<'_, '_>) -> String {
    node.children()
        .filter(roxmltree::Node::is_text)
        .filter_map(|child| child.text())
        .collect()
}

/// `encoding/xml`'s `copyValue` for integer fields: empty text is zero, otherwise the
/// trimmed text must parse in base 10.
fn go_int<T: std::str::FromStr + Default>(node: roxmltree::Node<'_, '_>) -> Result<T> {
    let raw = text(node);
    if raw.is_empty() {
        return Ok(T::default());
    }
    let trimmed = raw.trim();
    let valid = trimmed
        .strip_prefix(['-', '+'])
        .unwrap_or(trimmed)
        .bytes()
        .all(|b| b.is_ascii_digit());
    trimmed
        .parse()
        .ok()
        .filter(|_| valid && !trimmed.is_empty())
        .ok_or_else(|| {
            Error::Decode(format!(
                "strconv.ParseInt: parsing {}: invalid syntax",
                go_quote(trimmed)
            ))
        })
}

/// Go's `path` package functions the package readers need.
pub(crate) mod go_path {
    /// `path.Clean`.
    pub(crate) fn clean(path: &str) -> String {
        if path.is_empty() {
            return ".".to_owned();
        }
        let rooted = path.starts_with('/');
        let mut parts: Vec<&str> = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.last().is_some_and(|last| *last != "..") {
                        parts.pop();
                    } else if !rooted {
                        parts.push("..");
                    }
                }
                _ => parts.push(part),
            }
        }
        let joined = parts.join("/");
        match (rooted, joined.is_empty()) {
            (true, _) => format!("/{joined}"),
            (false, true) => ".".to_owned(),
            (false, false) => joined,
        }
    }

    /// `path.Join(a, b)`.
    pub(crate) fn join(a: &str, b: &str) -> String {
        let parts: Vec<&str> = [a, b].into_iter().filter(|part| !part.is_empty()).collect();
        if parts.is_empty() {
            return String::new();
        }
        clean(&parts.join("/"))
    }

    /// `path.Base`.
    pub(crate) fn base(path: &str) -> String {
        if path.is_empty() {
            return ".".to_owned();
        }
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return "/".to_owned();
        }
        trimmed
            .rsplit_once('/')
            .map_or(trimmed, |(_, base)| base)
            .to_owned()
    }

    /// `path.Dir`.
    pub(crate) fn dir(path: &str) -> String {
        let directory = path.rfind('/').map_or("", |index| &path[..=index]);
        clean(directory)
    }
}

#[cfg(test)]
pub(crate) mod tests;
