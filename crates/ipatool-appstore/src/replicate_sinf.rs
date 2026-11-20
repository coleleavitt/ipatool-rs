//! Go's `appstore_replicate_sinf.go`: rewrites the package with the sinfs Apple returned.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::download_types::Sinf;
use crate::zipfile::{self, Archive, Writer};
use crate::{Client, Error, Result};

/// Go's `packageManifest`.
fn manifest_sinf_paths(data: &[u8]) -> Result<Vec<String>> {
    let value: plist::Value = plist::from_bytes(data)
        .map_err(|error| Error::Invalid(format!("failed to unmarshal data: {error}")))?;
    Ok(value
        .as_dictionary()
        .and_then(|dict| dict.get("SinfPaths"))
        .and_then(plist::Value::as_array)
        .map_or_else(Vec::new, |paths| {
            paths
                .iter()
                .map(|path| path.as_string().unwrap_or_default().to_owned())
                .collect()
        }))
}

/// Go's `packageInfo`.
fn bundle_executable(data: &[u8]) -> Result<String> {
    let value: plist::Value = plist::from_bytes(data)
        .map_err(|error| Error::Invalid(format!("failed to unmarshal data: {error}")))?;
    Ok(value
        .as_dictionary()
        .and_then(|dict| dict.get("CFBundleExecutable"))
        .and_then(plist::Value::as_string)
        .unwrap_or_default()
        .to_owned())
}

fn read_entry(archive: &Archive, matches: impl Fn(&str) -> bool) -> Result<Option<Vec<u8>>> {
    let Some(file) = archive.files.iter().find(|file| matches(&file.name)) else {
        return Ok(None);
    };
    archive
        .read(file)
        .map(Some)
        .map_err(|error| Error::io("failed to copy data", error))
}

/// Go's `readBundleName`.
pub(crate) fn read_bundle_name(archive: &Archive) -> Result<String> {
    archive
        .files
        .iter()
        .find(|file| file.name.contains(".app/Info.plist") && !file.name.contains("/Watch/"))
        .and_then(|file| {
            let trimmed = file.name.strip_suffix(".app/Info.plist")?;
            Some(go_base(trimmed))
        })
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::Invalid("could not read bundle name".to_owned()))
}

/// Go's `filepath.Base` on a slash path.
fn go_base(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.is_empty() {
            ".".to_owned()
        } else {
            "/".to_owned()
        };
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_owned()
}

fn io_context(context: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |error| Error::io(context, error)
}

/// Go's `writeReplicatedZip`.
fn write_replicated_zip(sinfs: &[Sinf], package: &Path, tmp: &Path) -> Result<()> {
    let archive = Archive::open(package)
        .map_err(|_| Error::Invalid("failed to open zip reader".to_owned()))?;
    // Go opens the temporary file without O_TRUNC; truncating keeps a stale, longer file
    // from leaving bytes after the new end record.
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(tmp)
        .map_err(io_context("failed to open file"))?;
    let mut writer = Writer::new(file);
    zipfile::replicate(&archive, &mut writer).map_err(io_context("failed to replicate zip"))?;
    // Device-based downloads can omit sinfs; keep the rewrite, inject only supplied data.
    if !sinfs.is_empty() {
        add_sinfs(&archive, &mut writer, sinfs)
            .map_err(|error| error.context("failed to replicate sinf"))?;
    }
    let mut file = writer
        .finish()
        .map_err(io_context("failed to close zip writer"))?;
    file.flush()
        .map_err(io_context("failed to close replicated file"))?;
    Ok(())
}

fn add_sinfs(archive: &Archive, writer: &mut Writer<impl Write>, sinfs: &[Sinf]) -> Result<()> {
    let bundle =
        read_bundle_name(archive).map_err(|error| error.context("failed to read bundle name"))?;
    let manifest = read_entry(archive, |name| {
        name.ends_with(".app/SC_Info/Manifest.plist")
    })
    .and_then(|data| data.map(|data| manifest_sinf_paths(&data)).transpose())
    .map_err(|error| error.context("failed to read manifest plist"))?;
    let info = read_entry(archive, |name| name.contains(".app/Info.plist"))
        .and_then(|data| data.map(|data| bundle_executable(&data)).transpose())
        .map_err(|error| error.context("failed to read info plist"))?;
    let entries: Vec<(String, &[u8])> = if let Some(paths) = manifest {
        if paths.len() != sinfs.len() {
            return Err(Error::Invalid("slices have different lengths".to_owned())
                .context("failed to zip sinfs"));
        }
        sinfs
            .iter()
            .zip(paths)
            .map(|(sinf, path)| (format!("Payload/{bundle}.app/{path}"), sinf.data.as_slice()))
            .collect()
    } else {
        // Go dereferences the Info.plist result here and panics without one.
        let executable =
            info.ok_or_else(|| Error::Invalid("package has no Info.plist".to_owned()))?;
        vec![(
            format!("Payload/{bundle}.app/SC_Info/{executable}.sinf"),
            sinfs[0].data.as_slice(),
        )]
    };
    for (path, data) in entries {
        writer
            .create(&path)
            .map_err(io_context("failed to create file"))?;
        writer
            .write_all(data)
            .map_err(io_context("failed to write data"))?;
    }
    Ok(())
}

/// Go's `ReplicateSinf`, blocking.
pub(crate) fn replicate_sinf(sinfs: &[Sinf], package: &Path) -> Result<()> {
    let mut tmp = package.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    write_replicated_zip(sinfs, package, &tmp)?;
    fs::remove_file(package).map_err(io_context("failed to remove original file"))?;
    fs::rename(&tmp, package).map_err(io_context("failed to rename replicated file"))
}

impl Client {
    /// Go's `ReplicateSinf`: rewrites the package in place with the sinfs at the paths its
    /// `SC_Info/Manifest.plist` lists (or `SC_Info/<executable>.sinf`).
    ///
    /// # Errors
    /// Go's zip, plist and file errors.
    pub async fn replicate_sinf(&self, sinfs: Vec<Sinf>, package: PathBuf) -> Result<()> {
        tokio::task::spawn_blocking(move || replicate_sinf(&sinfs, &package))
            .await
            .map_err(|error| Error::Invalid(format!("replicate sinf task failed: {error}")))?
    }
}
