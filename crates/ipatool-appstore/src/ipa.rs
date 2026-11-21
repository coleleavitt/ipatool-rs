//! A small public facade over the crate's zip reader and writer for reading an IPA's
//! members and rewriting it with some members replaced. The `FairPlay` decryptor
//! (`ipatool-decrypt`) uses it instead of its own zip code.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use crate::zipfile::{self, Archive, Writer};

/// An opened IPA (a zip archive) read from a file on disk.
pub struct IpaReader {
    archive: Archive,
}

impl IpaReader {
    /// Opens the IPA at `path` and reads its central directory.
    ///
    /// # Errors
    /// If the file cannot be read or is not a valid zip archive.
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            archive: Archive::open(path)?,
        })
    }

    /// The member names in central-directory order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.archive
            .files
            .iter()
            .map(|file| file.name.as_str())
            .collect()
    }

    /// The decompressed contents of the first member named `name`, or `None` when the
    /// archive has no such member.
    ///
    /// # Errors
    /// If the member cannot be decompressed or fails its checksum.
    pub fn read(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let Some(header) = self.archive.files.iter().find(|file| file.name == name) else {
            return Ok(None);
        };
        self.archive.read(header).map(Some)
    }

    /// The decompressed contents of every member with its name, in central-directory
    /// order. Each member is read when the iterator reaches it.
    pub fn members(&self) -> impl Iterator<Item = (&str, io::Result<Vec<u8>>)> {
        self.archive
            .files
            .iter()
            .map(|file| (file.name.as_str(), self.archive.read(file)))
    }

    /// Writes the IPA to `out`, copying every member for which `keep` is true with its
    /// compressed bytes, except the ones in `replacements`, whose decompressed contents
    /// are written in their place (deflated). Member order is preserved.
    ///
    /// # Errors
    /// If a source member cannot be read or the output cannot be written.
    pub fn rewrite<W: Write>(
        &self,
        out: W,
        replacements: &BTreeMap<String, Vec<u8>>,
        keep: impl Fn(&str) -> bool,
    ) -> io::Result<()> {
        let mut writer = Writer::new(out);
        for file in &self.archive.files {
            if !keep(&file.name) {
                continue;
            }
            if let Some(data) = replacements.get(&file.name) {
                writer.create(&file.name)?;
                writer.write_all(data)?;
                continue;
            }
            let local_extra = source_local_extra(&self.archive, file)?;
            let mut header = file.clone();
            header.extra = zipfile::stored_extra(&header, &local_extra);
            if header.name.ends_with('/') || header.method == zipfile::STORE {
                header.flags &= !0x8;
            } else if header.method == zipfile::DEFLATE {
                header.flags |= 0x8;
            }
            let raw = self.archive.open_raw(file)?;
            writer.create_raw(header, zipfile::extra_without_zip64(&file.extra))?;
            if !file.name.ends_with('/') {
                writer.copy_from(raw)?;
            }
        }
        writer.finish()?;
        Ok(())
    }
}

fn source_local_extra(archive: &Archive, file: &zipfile::FileHeader) -> io::Result<Vec<u8>> {
    archive.local_extra(file).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read local metadata for {:?}: {error}", file.name),
        )
    })
}
