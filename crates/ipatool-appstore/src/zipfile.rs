//! The parts of Go's `archive/zip` the package rewrite relies on, with Go's framing: the
//! central-directory reader (`reader.go`), raw entry copies and new deflated entries
//! (`writer.go`), plus `appstore_zip_headers.go`'s local-extra lookup. Entries are copied
//! with their compressed bytes, so their framing must follow Go's writer exactly.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use flate2::Compression;
use flate2::write::DeflateEncoder;

const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const END_SIGNATURE: u32 = 0x0605_4b50;
const END64_SIGNATURE: u32 = 0x0606_4b50;
const LOCATOR64_SIGNATURE: u32 = 0x0706_4b50;
const DESCRIPTOR_SIGNATURE: u32 = 0x0807_4b50;
pub(crate) const ZIP64_EXTRA_ID: u16 = 0x0001;
const LOCAL_LEN: usize = 30;
const CENTRAL_LEN: usize = 46;
const END_LEN: usize = 22;
const END64_LEN: usize = 56;
const LOCATOR64_LEN: usize = 20;
const MAX32: u64 = 0xffff_ffff;
pub(crate) const STORE: u16 = 0;
pub(crate) const DEFLATE: u16 = 8;
const VERSION_20: u16 = 20;
const VERSION_45: u16 = 45;

/// `zip.ErrFormat`.
pub(crate) fn format_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "zip: not a valid zip file")
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// Go's `zip.FileHeader` (with the reader's `headerOffset`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileHeader {
    pub name: String,
    pub comment: Vec<u8>,
    pub creator_version: u16,
    pub reader_version: u16,
    pub flags: u16,
    pub method: u16,
    pub modified_time: u16,
    pub modified_date: u16,
    pub crc32: u32,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub compressed_size64: u64,
    pub uncompressed_size64: u64,
    pub extra: Vec<u8>,
    pub external_attrs: u32,
    pub header_offset: u64,
}

impl FileHeader {
    fn is_zip64(&self) -> bool {
        self.compressed_size64 >= MAX32 || self.uncompressed_size64 >= MAX32
    }

    fn has_data_descriptor(&self) -> bool {
        self.flags & 0x8 != 0
    }
}

/// Go's `zip.ReadCloser`: the central directory of an archive on disk.
pub(crate) struct Archive {
    file: File,
    pub files: Vec<FileHeader>,
}

struct DirectoryEnd {
    records: u64,
    size: u64,
    offset: u64,
    end_offset: u64,
}

fn read_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
    let mut file = file;
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buffer)
}

/// Go's `readDirectoryEnd` and `readDirectory64End`.
fn read_directory_end(file: &File, size: u64) -> io::Result<DirectoryEnd> {
    let mut found = None;
    for search in [1024_u64, 65 * 1024] {
        let search = search.min(size);
        let mut tail = vec![0; usize::try_from(search).map_err(|_| format_error())?];
        read_at(file, size - search, &mut tail)?;
        if let Some(position) = find_signature(&tail) {
            found = Some((size - search + position as u64, tail[position..].to_vec()));
            break;
        }
        if search == size {
            break;
        }
    }
    let (end_offset, record) = found.ok_or_else(format_error)?;
    let mut end = DirectoryEnd {
        records: u64::from(u16_at(&record, 10)),
        size: u64::from(u32_at(&record, 12)),
        offset: u64::from(u32_at(&record, 16)),
        end_offset,
    };
    // Go compares the directory size with 0xffff here, not 0xffffffff.
    if (end.records == 0xffff || end.size == 0xffff || end.offset == MAX32)
        && let Some(offset) = find_directory64_end(file, end_offset)?
    {
        let mut record = [0; END64_LEN];
        read_at(file, offset, &mut record)?;
        if u32_at(&record, 0) != END64_SIGNATURE {
            return Err(format_error());
        }
        end.records = u64_at(&record, 32);
        end.size = u64_at(&record, 40);
        end.offset = u64_at(&record, 48);
        end.end_offset = offset;
    }
    Ok(end)
}

/// Go's `findDirectory64End`: the zip64 end record's offset from its locator, if any.
fn find_directory64_end(file: &File, end_offset: u64) -> io::Result<Option<u64>> {
    let Some(at) = end_offset.checked_sub(LOCATOR64_LEN as u64) else {
        return Ok(None);
    };
    let mut locator = [0; LOCATOR64_LEN];
    read_at(file, at, &mut locator)?;
    if u32_at(&locator, 0) != LOCATOR64_SIGNATURE
        || u32_at(&locator, 4) != 0
        || u32_at(&locator, 16) != 1
    {
        return Ok(None);
    }
    Ok(Some(u64_at(&locator, 8)))
}

/// Go's `findSignatureInBlock`: the last end record whose comment fits.
fn find_signature(block: &[u8]) -> Option<usize> {
    if block.len() < END_LEN {
        return None;
    }
    (0..=block.len() - END_LEN).rev().find(|&i| {
        u32_at(block, i) == END_SIGNATURE
            && i + END_LEN + usize::from(u16_at(block, i + 20)) <= block.len()
    })
}

impl Archive {
    /// Go's `zip.OpenReader`.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        let end = read_directory_end(&file, size)?;
        // Go's baseOffset: support archives with a prefix, unless the declared offset
        // already points at a central header.
        let mut base = end
            .end_offset
            .checked_sub(end.size)
            .and_then(|start| start.checked_sub(end.offset))
            .ok_or_else(format_error)?;
        if base > 0 {
            let mut signature = [0; 4];
            if read_at(&file, end.offset, &mut signature).is_ok()
                && u32_at(&signature, 0) == CENTRAL_SIGNATURE
            {
                base = 0;
            }
        }
        let start = base + end.offset;
        if start >= size {
            return Err(format_error());
        }
        // Go reads central records from the directory start until one does not parse.
        let mut directory = vec![0; usize::try_from(size - start).map_err(|_| format_error())?];
        read_at(&file, start, &mut directory)?;
        let mut files = Vec::new();
        let mut rest = directory.as_slice();
        let mut failure = format_error();
        loop {
            if rest.len() < 4 || u32_at(rest, 0) != CENTRAL_SIGNATURE {
                break;
            }
            match read_directory_header(rest) {
                Ok((mut header, used)) => {
                    header.header_offset += base;
                    files.push(header);
                    rest = &rest[used..];
                }
                Err(error) => {
                    failure = error;
                    break;
                }
            }
        }
        // Go accepts a record count that only matches modulo 2^16.
        if files.len() as u64 % 0x1_0000 != end.records % 0x1_0000 {
            return Err(failure);
        }
        Ok(Self { file, files })
    }

    /// Go's `File.DataOffset`.
    pub(crate) fn data_offset(&self, header: &FileHeader) -> io::Result<u64> {
        let mut local = [0; LOCAL_LEN];
        read_at(&self.file, header.header_offset, &mut local)?;
        if u32_at(&local, 0) != LOCAL_SIGNATURE {
            return Err(format_error());
        }
        let lengths = u64::from(u16_at(&local, 26)) + u64::from(u16_at(&local, 28));
        Ok(header.header_offset + LOCAL_LEN as u64 + lengths)
    }

    /// `zipLocalHeaders.extra`: the local header's extra field, after checking that the
    /// local name matches the central one.
    pub(crate) fn local_extra(&self, header: &FileHeader) -> io::Result<Vec<u8>> {
        let mut local = [0; LOCAL_LEN];
        read_at(&self.file, header.header_offset, &mut local)?;
        if u32_at(&local, 0) != LOCAL_SIGNATURE {
            return Err(format_error());
        }
        let name_len = usize::from(u16_at(&local, 26));
        let extra_len = usize::from(u16_at(&local, 28));
        let mut metadata = vec![0; name_len + extra_len];
        read_at(
            &self.file,
            header.header_offset + LOCAL_LEN as u64,
            &mut metadata,
        )?;
        if metadata[..name_len] != *header.name.as_bytes() {
            return Err(format_error());
        }
        Ok(metadata.split_off(name_len))
    }

    /// Go's `File.OpenRaw`.
    pub(crate) fn open_raw(&self, header: &FileHeader) -> io::Result<impl Read + '_> {
        let offset = self.data_offset(header)?;
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file.take(header.compressed_size64))
    }

    /// Go's `File.Open` + `io.ReadAll`, with its size and checksum checks.
    pub(crate) fn read(&self, header: &FileHeader) -> io::Result<Vec<u8>> {
        let raw = self.open_raw(header)?;
        let mut data = Vec::new();
        match header.method {
            STORE => {
                raw.take(header.uncompressed_size64)
                    .read_to_end(&mut data)?;
            }
            DEFLATE => {
                flate2::read::DeflateDecoder::new(raw)
                    .take(header.uncompressed_size64 + 1)
                    .read_to_end(&mut data)?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "zip: unsupported compression algorithm",
                ));
            }
        }
        if data.len() as u64 != header.uncompressed_size64 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF",
            ));
        }
        if crc32fast::hash(&data) != header.crc32 && header.crc32 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "zip: checksum error",
            ));
        }
        Ok(data)
    }
}

/// Go's `readDirectoryHeader`: one central record and the bytes it used.
fn read_directory_header(data: &[u8]) -> io::Result<(FileHeader, usize)> {
    if data.len() < CENTRAL_LEN {
        return Err(format_error());
    }
    let name_len = usize::from(u16_at(data, 28));
    let extra_len = usize::from(u16_at(data, 30));
    let comment_len = usize::from(u16_at(data, 32));
    let used = CENTRAL_LEN + name_len + extra_len + comment_len;
    if data.len() < used {
        return Err(format_error());
    }
    let variable = &data[CENTRAL_LEN..used];
    let compressed_size = u32_at(data, 20);
    let uncompressed_size = u32_at(data, 24);
    let mut header = FileHeader {
        creator_version: u16_at(data, 4),
        reader_version: u16_at(data, 6),
        flags: u16_at(data, 8),
        method: u16_at(data, 10),
        modified_time: u16_at(data, 12),
        modified_date: u16_at(data, 14),
        crc32: u32_at(data, 16),
        compressed_size,
        uncompressed_size,
        compressed_size64: u64::from(compressed_size),
        uncompressed_size64: u64::from(uncompressed_size),
        external_attrs: u32_at(data, 38),
        header_offset: u64::from(u32_at(data, 42)),
        name: String::from_utf8_lossy(&variable[..name_len]).into_owned(),
        extra: variable[name_len..name_len + extra_len].to_vec(),
        comment: variable[name_len + extra_len..].to_vec(),
    };
    let mut need_uncompressed = uncompressed_size == u32::MAX;
    let mut need_compressed = compressed_size == u32::MAX;
    let mut need_offset = header.header_offset == MAX32;
    let mut extra = header.extra.as_slice();
    while extra.len() >= 4 {
        let tag = u16_at(extra, 0);
        let size = usize::from(u16_at(extra, 2));
        if extra.len() - 4 < size {
            break;
        }
        let mut field = &extra[4..4 + size];
        extra = &extra[4 + size..];
        if tag != ZIP64_EXTRA_ID {
            continue;
        }
        let mut next = |needed: &mut bool| -> io::Result<Option<u64>> {
            if !*needed {
                return Ok(None);
            }
            *needed = false;
            if field.len() < 8 {
                return Err(format_error());
            }
            let value = u64_at(field, 0);
            field = &field[8..];
            Ok(Some(value))
        };
        if let Some(value) = next(&mut need_uncompressed)? {
            header.uncompressed_size64 = value;
        }
        if let Some(value) = next(&mut need_compressed)? {
            header.compressed_size64 = value;
        }
        if let Some(value) = next(&mut need_offset)? {
            header.header_offset = value;
        }
    }
    // Go accepts an uncompressed size of 2^32-1 without a zip64 field (42.zip).
    if need_compressed || need_offset {
        return Err(format_error());
    }
    Ok((header, used))
}

/// `zipExtraWithoutZIP64`: the extra field without zip64 blocks; others verbatim.
pub(crate) fn extra_without_zip64(mut extra: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(extra.len());
    while extra.len() >= 4 {
        let length = usize::from(u16_at(extra, 2)) + 4;
        if length > extra.len() {
            break;
        }
        if u16_at(extra, 0) != ZIP64_EXTRA_ID {
            result.extend_from_slice(&extra[..length]);
        }
        extra = &extra[length..];
    }
    result.extend_from_slice(extra);
    result
}

/// `zipStoredExtra`: stored zip64 entries carry authoritative local sizes.
pub(crate) fn stored_extra(header: &FileHeader, extra: &[u8]) -> Vec<u8> {
    if header.method != STORE
        || (header.compressed_size64 < MAX32 && header.uncompressed_size64 < MAX32)
    {
        return extra.to_vec();
    }
    let mut result = extra_without_zip64(extra);
    result.extend_from_slice(&ZIP64_EXTRA_ID.to_le_bytes());
    result.extend_from_slice(&16_u16.to_le_bytes());
    result.extend_from_slice(&header.uncompressed_size64.to_le_bytes());
    result.extend_from_slice(&header.compressed_size64.to_le_bytes());
    result
}

struct CountWriter<W> {
    inner: W,
    count: u64,
}

impl<W: Write> Write for CountWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(data)?;
        self.count += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct Entry {
    header: FileHeader,
    offset: u64,
}

/// The entry being written: raw bytes, or data Go's writer compresses.
enum Open {
    Raw,
    Deflate {
        encoder: Box<DeflateEncoder<Vec<u8>>>,
        crc: crc32fast::Hasher,
        size: u64,
    },
}

/// Go's `zip.Writer` for `CreateRaw` and `Create`.
pub(crate) struct Writer<W: Write> {
    out: CountWriter<BufWriter<W>>,
    dir: Vec<Entry>,
    open: Option<Open>,
}

impl<W: Write> Writer<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            out: CountWriter {
                inner: BufWriter::new(inner),
                count: 0,
            },
            dir: Vec::new(),
            open: None,
        }
    }

    /// Go's `CreateRaw`: writes the local header now; the caller copies the compressed
    /// bytes with [`Writer::write`]. `central_extra` is the extra field the central
    /// directory gets, which Go swaps in after the local header is written.
    pub(crate) fn create_raw(
        &mut self,
        mut header: FileHeader,
        central_extra: Vec<u8>,
    ) -> io::Result<()> {
        self.close_entry()?;
        header.compressed_size =
            u32::try_from(header.compressed_size64.min(MAX32)).unwrap_or(u32::MAX);
        header.uncompressed_size =
            u32::try_from(header.uncompressed_size64.min(MAX32)).unwrap_or(u32::MAX);
        let offset = self.out.count;
        write_local_header(&mut self.out, &header, true)?;
        header.extra = central_extra;
        let directory = header.name.ends_with('/');
        self.dir.push(Entry { header, offset });
        self.open = (!directory).then_some(Open::Raw);
        Ok(())
    }

    /// Go's `Create`: a deflated entry with a data descriptor and no timestamp.
    pub(crate) fn create(&mut self, name: &str) -> io::Result<()> {
        self.close_entry()?;
        let mut header = FileHeader {
            name: name.to_owned(),
            method: DEFLATE,
            creator_version: VERSION_20,
            reader_version: VERSION_20,
            flags: 0x8,
            ..FileHeader::default()
        };
        // Go sets the UTF-8 flag only for names that need it.
        if !name.is_ascii() {
            header.flags |= 0x800;
        }
        let offset = self.out.count;
        write_local_header(&mut self.out, &header, false)?;
        self.dir.push(Entry { header, offset });
        self.open = Some(Open::Deflate {
            encoder: Box::new(DeflateEncoder::new(Vec::new(), Compression::default())),
            crc: crc32fast::Hasher::new(),
            size: 0,
        });
        Ok(())
    }

    /// Writes to the open entry.
    pub(crate) fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match &mut self.open {
            Some(Open::Raw) => self.out.write_all(data),
            Some(Open::Deflate { encoder, crc, size }) => {
                crc.update(data);
                *size += data.len() as u64;
                encoder.write_all(data)
            }
            // Go's dirWriter accepts empty writes.
            None if data.is_empty() => Ok(()),
            None => Err(io::Error::other("zip: write data to directory")),
        }
    }

    /// Copies a reader into the open entry.
    pub(crate) fn copy_from(&mut self, mut reader: impl Read) -> io::Result<u64> {
        let mut buffer = vec![0; 64 * 1024];
        let mut total = 0;
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                return Ok(total);
            }
            self.write_all(&buffer[..read])?;
            total += read as u64;
        }
    }

    /// Go's `fileWriter.close`.
    fn close_entry(&mut self) -> io::Result<()> {
        let Some(open) = self.open.take() else {
            return Ok(());
        };
        let Some(entry) = self.dir.last_mut() else {
            return Ok(());
        };
        let header = &mut entry.header;
        if let Open::Deflate { encoder, crc, size } = open {
            let compressed = encoder.finish()?;
            self.out.write_all(&compressed)?;
            header.crc32 = crc.finalize();
            header.compressed_size64 = compressed.len() as u64;
            header.uncompressed_size64 = size;
            if header.is_zip64() {
                header.compressed_size = u32::MAX;
                header.uncompressed_size = u32::MAX;
                header.reader_version = VERSION_45;
            } else {
                header.compressed_size =
                    u32::try_from(header.compressed_size64).unwrap_or(u32::MAX);
                header.uncompressed_size =
                    u32::try_from(header.uncompressed_size64).unwrap_or(u32::MAX);
            }
        }
        if !header.has_data_descriptor() {
            return Ok(());
        }
        let mut buffer = Vec::with_capacity(24);
        buffer.extend_from_slice(&DESCRIPTOR_SIGNATURE.to_le_bytes());
        buffer.extend_from_slice(&header.crc32.to_le_bytes());
        if header.is_zip64() {
            buffer.extend_from_slice(&header.compressed_size64.to_le_bytes());
            buffer.extend_from_slice(&header.uncompressed_size64.to_le_bytes());
        } else {
            buffer.extend_from_slice(&header.compressed_size.to_le_bytes());
            buffer.extend_from_slice(&header.uncompressed_size.to_le_bytes());
        }
        self.out.write_all(&buffer)
    }

    /// Go's `Writer.Close`: the central directory and end records.
    pub(crate) fn finish(mut self) -> io::Result<W> {
        self.close_entry()?;
        let start = self.out.count;
        for entry in &mut self.dir {
            let header = &mut entry.header;
            let mut record = Vec::with_capacity(CENTRAL_LEN);
            record.extend_from_slice(&CENTRAL_SIGNATURE.to_le_bytes());
            for value in [
                header.creator_version,
                header.reader_version,
                header.flags,
                header.method,
                header.modified_time,
                header.modified_date,
            ] {
                record.extend_from_slice(&value.to_le_bytes());
            }
            record.extend_from_slice(&header.crc32.to_le_bytes());
            if header.is_zip64() || entry.offset >= MAX32 {
                record.extend_from_slice(&u32::MAX.to_le_bytes());
                record.extend_from_slice(&u32::MAX.to_le_bytes());
                header
                    .extra
                    .extend_from_slice(&ZIP64_EXTRA_ID.to_le_bytes());
                header.extra.extend_from_slice(&24_u16.to_le_bytes());
                header
                    .extra
                    .extend_from_slice(&header.uncompressed_size64.to_le_bytes());
                header
                    .extra
                    .extend_from_slice(&header.compressed_size64.to_le_bytes());
                header.extra.extend_from_slice(&entry.offset.to_le_bytes());
            } else {
                record.extend_from_slice(&header.compressed_size.to_le_bytes());
                record.extend_from_slice(&header.uncompressed_size.to_le_bytes());
            }
            let lengths = [header.name.len(), header.extra.len(), header.comment.len()];
            for length in lengths {
                record.extend_from_slice(&length_u16(length)?.to_le_bytes());
            }
            record.extend_from_slice(&[0; 4]);
            record.extend_from_slice(&header.external_attrs.to_le_bytes());
            let offset = u32::try_from(entry.offset.min(MAX32)).unwrap_or(u32::MAX);
            record.extend_from_slice(&offset.to_le_bytes());
            self.out.write_all(&record)?;
            self.out.write_all(header.name.as_bytes())?;
            self.out.write_all(&header.extra)?;
            self.out.write_all(&header.comment)?;
        }
        let end = self.out.count;
        let mut records = self.dir.len() as u64;
        let mut size = end - start;
        let mut offset = start;
        if records >= 0xffff || size >= MAX32 || offset >= MAX32 {
            let mut record = Vec::with_capacity(END64_LEN + LOCATOR64_LEN);
            record.extend_from_slice(&END64_SIGNATURE.to_le_bytes());
            record.extend_from_slice(&(END64_LEN as u64 - 12).to_le_bytes());
            record.extend_from_slice(&VERSION_45.to_le_bytes());
            record.extend_from_slice(&VERSION_45.to_le_bytes());
            record.extend_from_slice(&[0; 8]);
            record.extend_from_slice(&records.to_le_bytes());
            record.extend_from_slice(&records.to_le_bytes());
            record.extend_from_slice(&size.to_le_bytes());
            record.extend_from_slice(&offset.to_le_bytes());
            record.extend_from_slice(&LOCATOR64_SIGNATURE.to_le_bytes());
            record.extend_from_slice(&0_u32.to_le_bytes());
            record.extend_from_slice(&end.to_le_bytes());
            record.extend_from_slice(&1_u32.to_le_bytes());
            self.out.write_all(&record)?;
            records = 0xffff;
            size = MAX32;
            offset = MAX32;
        }
        let mut record = Vec::with_capacity(END_LEN);
        record.extend_from_slice(&END_SIGNATURE.to_le_bytes());
        record.extend_from_slice(&[0; 4]);
        let records = u16::try_from(records).unwrap_or(u16::MAX);
        record.extend_from_slice(&records.to_le_bytes());
        record.extend_from_slice(&records.to_le_bytes());
        record.extend_from_slice(&u32::try_from(size).unwrap_or(u32::MAX).to_le_bytes());
        record.extend_from_slice(&u32::try_from(offset).unwrap_or(u32::MAX).to_le_bytes());
        record.extend_from_slice(&0_u16.to_le_bytes());
        self.out.write_all(&record)?;
        self.out
            .inner
            .into_inner()
            .map_err(io::IntoInnerError::into_error)
    }
}

fn length_u16(length: usize) -> io::Result<u16> {
    u16::try_from(length).map_err(|_| io::Error::other("zip: FileHeader.Extra too long"))
}

/// Go's `writeHeader`. Raw entries without a descriptor carry their sizes here.
fn write_local_header(out: &mut impl Write, header: &FileHeader, raw: bool) -> io::Result<()> {
    if header.name.len() > usize::from(u16::MAX) {
        return Err(io::Error::other("zip: FileHeader.Name too long"));
    }
    let mut record = Vec::with_capacity(LOCAL_LEN);
    record.extend_from_slice(&LOCAL_SIGNATURE.to_le_bytes());
    for value in [
        header.reader_version,
        header.flags,
        header.method,
        header.modified_time,
        header.modified_date,
    ] {
        record.extend_from_slice(&value.to_le_bytes());
    }
    if raw && !header.has_data_descriptor() {
        record.extend_from_slice(&header.crc32.to_le_bytes());
        record.extend_from_slice(&header.compressed_size.to_le_bytes());
        record.extend_from_slice(&header.uncompressed_size.to_le_bytes());
    } else {
        record.extend_from_slice(&[0; 12]);
    }
    record.extend_from_slice(&length_u16(header.name.len())?.to_le_bytes());
    record.extend_from_slice(&length_u16(header.extra.len())?.to_le_bytes());
    out.write_all(&record)?;
    out.write_all(header.name.as_bytes())?;
    out.write_all(&header.extra)
}

/// Go's `replicateZip`: every entry copied with its compressed bytes; deflated entries get
/// a trailing descriptor for streaming installers, stored entries inline sizes.
pub(crate) fn replicate(source: &Archive, writer: &mut Writer<impl Write>) -> io::Result<()> {
    for file in &source.files {
        let local_extra = source.local_extra(file).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to read local metadata for {:?}: {error}", file.name),
            )
        })?;
        let mut header = file.clone();
        header.extra = stored_extra(&header, &local_extra);
        if header.name.ends_with('/') || header.method == STORE {
            header.flags &= !0x8;
        } else if header.method == DEFLATE {
            header.flags |= 0x8;
        }
        let raw = source.open_raw(file)?;
        writer.create_raw(header, extra_without_zip64(&file.extra))?;
        if !file.name.ends_with('/') {
            writer.copy_from(raw)?;
        }
    }
    Ok(())
}
