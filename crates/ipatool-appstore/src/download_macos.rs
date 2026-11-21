//! Port of Go `appstore_download_macos.go`, `appstore_download_macos_adapter.go` and
//! `appstore_download_macos_publish.go`: download a native macOS `.pkg`, decrypt it with
//! `StoreAgent`, check it is a valid XAR and publish it over the destination.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ipatool_sap::machine::StoreAgent;

use crate::download_types::{
    DownloadItem, DownloadOutput, Phase, ProgressHandle, Sinf, report_phase,
};
use crate::xar;
use crate::{Client, Error, Result};

pub(crate) const DP_INFO_SUFFIX: &str = ".dpInfo";
pub(crate) const HW_INFO_SUFFIX: &str = ".hwInfo";
pub(crate) const ENCRYPTED_STAGE_SUFFIX: &str = ".ipatool-encrypted";
pub(crate) const DECRYPTED_STAGE_SUFFIX: &str = ".ipatool-decrypted";
pub(crate) const BACKUP_SUFFIX: &str = ".ipatool-backup";

/// Go's `macPackageDecrypter`.
pub(crate) trait MacPackageDecrypter {
    fn decrypt(&mut self, dst: &mut dyn Write, src: &mut dyn Read) -> Result<u64>;
    fn close(&mut self) -> Result<()>;
}

/// Go's `macPackageDecrypterFactory(hardwareID, dpInfo)`. It runs on a blocking thread,
/// so the decrypter itself need not be `Send`.
pub(crate) type DecrypterFactory =
    dyn Fn(&[u8], &[u8]) -> Result<Box<dyn MacPackageDecrypter>> + Send + Sync;

/// The file operations Go routes through `operatingsystem.OperatingSystem` and its tests
/// replace. Errors carry Go's `*PathError`/`*LinkError` text.
pub(crate) trait FileSystem: Send + Sync {
    fn remove(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
}

pub(crate) struct OsFileSystem;

impl FileSystem for OsFileSystem {
    fn remove(&self, path: &Path) -> io::Result<()> {
        // Go's `os.Remove` also removes empty directories.
        std::fs::remove_file(path)
            .or_else(|err| match std::fs::remove_dir(path) {
                Ok(()) => Ok(()),
                Err(_) => Err(err),
            })
            .map_err(|err| go_path_error("remove", path, &err))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "rename {} {}: {}",
                    from.display(),
                    to.display(),
                    go_errno_text(&err)
                ),
            )
        })
    }
}

/// Go's `*os.PathError` text: `op path: errno text`.
pub(crate) fn go_path_error(op: &str, path: &Path, err: &io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!("{op} {}: {}", path.display(), go_errno_text(err)),
    )
}

/// Go's `syscall.Errno` strings are the C library's with a lowercase first letter.
fn go_errno_text(err: &io::Error) -> String {
    let text = err.to_string();
    let text = text
        .rsplit_once(" (os error ")
        .map_or(text.as_str(), |(message, _)| message);
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

/// Go's `joinCleanupError`.
pub(crate) fn join_cleanup_error(err: Option<Error>, message: &str, cleanup: Error) -> Error {
    let wrapped = cleanup.context(message);
    match err {
        None => wrapped,
        Some(err) => Error::Joined(vec![err, wrapped]),
    }
}

pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path = OsString::from(path);
    path.push(suffix);
    PathBuf::from(path)
}

impl Client {
    /// Go's `downloadMacPackage`.
    pub(crate) async fn download_mac_package(
        &self,
        item: &DownloadItem,
        destination: &Path,
        hardware_id: &[u8],
        progress: Option<ProgressHandle>,
        parallel: bool,
    ) -> Result<DownloadOutput> {
        MacPackageDownload::default()
            .run(
                item,
                destination,
                hardware_id,
                async |url: &str, path: &Path| {
                    self.download_file(url, path, progress.clone(), parallel)
                        .await?;
                    // Decryption follows the download in `run`.
                    report_phase(progress.as_ref(), Phase::Decrypting);
                    Ok(())
                },
            )
            .await
    }
}

/// The macOS package download with its replaceable parts (Go's `appstore.os` and
/// `appstore.macDecrypterFactory`).
#[derive(Clone)]
pub(crate) struct MacPackageDownload {
    pub fs: Arc<dyn FileSystem>,
    pub factory: Arc<DecrypterFactory>,
}

impl Default for MacPackageDownload {
    fn default() -> Self {
        Self {
            fs: Arc::new(OsFileSystem),
            factory: Arc::new(default_decrypter_factory),
        }
    }
}

/// Removes the staging files if the download is dropped before it finishes.
struct Staging {
    fs: Arc<dyn FileSystem>,
    paths: [PathBuf; 2],
    armed: bool,
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.armed {
            let _ = cleanup_paths(self.fs.as_ref(), &self.paths);
        }
    }
}

impl MacPackageDownload {
    /// Go's `downloadMacPackage`. `download(url, path)` is Go's `downloadFile`.
    pub(crate) async fn run(
        &self,
        item: &DownloadItem,
        destination: &Path,
        hardware_id: &[u8],
        download: impl AsyncFnOnce(&str, &Path) -> Result<()>,
    ) -> Result<DownloadOutput> {
        let dp_info = mac_dp_info(&item.sinfs)?;
        let encrypted_path = with_suffix(destination, ENCRYPTED_STAGE_SUFFIX);
        let decrypted_path = with_suffix(destination, DECRYPTED_STAGE_SUFFIX);
        let mut staging = Staging {
            fs: Arc::clone(&self.fs),
            paths: [encrypted_path.clone(), decrypted_path.clone()],
            armed: false,
        };
        cleanup_paths(self.fs.as_ref(), &staging.paths)
            .map_err(|err| err.context("failed to prepare macOS download staging"))?;
        staging.armed = true;

        if let Err(err) = download(&item.url, &encrypted_path).await {
            return Err(self.finish(&mut staging, err.context("failed to download file")));
        }

        // Decryption runs Apple's code in the emulator; keep it off the async workers. The
        // thread owns the staging cleanup from here on.
        let this = self.clone();
        let hardware_id = hardware_id.to_vec();
        let destination = destination.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let result = this.decrypt_and_publish(
                &hardware_id,
                &dp_info,
                &encrypted_path,
                &decrypted_path,
                &destination,
            );
            match result {
                Ok(()) => {
                    staging.armed = false;
                    let _ = cleanup_paths(this.fs.as_ref(), &staging.paths);
                    // The validated package is already published. Legacy sidecars are
                    // obsolete metadata, so their cleanup must not turn a successful
                    // package replacement into an error.
                    let _ = this.remove_legacy_sidecars(&destination);
                    Ok(DownloadOutput {
                        destination_path: destination,
                        sinfs: Vec::new(),
                    })
                }
                Err(err) => Err(this.finish(&mut staging, err)),
            }
        })
        .await
        .map_err(|err| Error::Invalid(format!("macOS package decryption task failed: {err}")))?
    }

    /// Go's deferred staging cleanup for an unpublished download.
    fn finish(&self, staging: &mut Staging, err: Error) -> Error {
        staging.armed = false;
        match cleanup_paths(self.fs.as_ref(), &staging.paths) {
            Ok(()) => err,
            Err(cleanup) => join_cleanup_error(
                Some(err),
                "failed to clean up macOS download staging",
                cleanup,
            ),
        }
    }

    fn decrypt_and_publish(
        &self,
        hardware_id: &[u8],
        dp_info: &[u8],
        encrypted_path: &Path,
        decrypted_path: &Path,
        destination: &Path,
    ) -> Result<()> {
        let decrypter = (self.factory)(hardware_id, dp_info)
            .map_err(|err| err.context("failed to initialize macOS package decrypter"))?;
        decrypt_package(decrypter, encrypted_path, decrypted_path)?;
        validate_package(decrypted_path)
            .map_err(|err| err.context("failed to validate decrypted macOS package"))?;
        self.publish(decrypted_path, destination)
    }

    fn remove_legacy_sidecars(&self, destination: &Path) -> Result<()> {
        cleanup_paths(
            self.fs.as_ref(),
            &[
                with_suffix(destination, DP_INFO_SUFFIX),
                with_suffix(destination, HW_INFO_SUFFIX),
            ],
        )
    }

    /// Go's `publishMacPackage`.
    fn publish(&self, staged: &Path, destination: &Path) -> Result<()> {
        let backup = backup_file(self.fs.as_ref(), destination)
            .map_err(|err| err.context("failed to preserve existing package"))?;
        if let Err(err) = self.fs.rename(staged, destination) {
            let publish = Error::Invalid(format!("failed to publish package: {err}"));
            return Err(
                match restore(self.fs.as_ref(), destination, backup.as_deref()) {
                    Ok(()) => publish,
                    Err(rollback) => join_cleanup_error(
                        Some(publish),
                        "failed to restore existing macOS download",
                        rollback,
                    ),
                },
            );
        }
        if let Some(backup) = backup {
            let _ = self.fs.remove(&backup);
        }
        Ok(())
    }
}

/// Go's `decryptMacPackage`: the decrypter is closed on every path, after both files.
fn decrypt_package(
    mut decrypter: Box<dyn MacPackageDecrypter>,
    encrypted_path: &Path,
    decrypted_path: &Path,
) -> Result<()> {
    let result = decrypt_files(decrypter.as_mut(), encrypted_path, decrypted_path);
    match decrypter.close() {
        Ok(()) => result,
        Err(close) => Err(join_cleanup_error(
            result.err(),
            "failed to close macOS package decrypter",
            close,
        )),
    }
}

fn decrypt_files(
    decrypter: &mut dyn MacPackageDecrypter,
    encrypted_path: &Path,
    decrypted_path: &Path,
) -> Result<()> {
    let mut source = std::fs::File::open(encrypted_path).map_err(|err| {
        Error::Invalid(format!(
            "failed to open encrypted macOS package: {}",
            go_path_error("open", encrypted_path, &err)
        ))
    })?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut destination = options.open(decrypted_path).map_err(|err| {
        Error::Invalid(format!(
            "failed to open decrypted macOS package staging: {}",
            go_path_error("open", decrypted_path, &err)
        ))
    })?;
    decrypter
        .decrypt(&mut destination, &mut source)
        .map(drop)
        .map_err(|err| err.context("failed to decrypt macOS package"))
}

/// Go's `validateMacPackage`: a non-empty XAR whose entries match their checksums.
fn validate_package(path: &Path) -> Result<()> {
    let file = std::fs::File::open(path).map_err(|err| {
        Error::Invalid(format!(
            "failed to open decrypted macOS package: {}",
            go_path_error("open", path, &err)
        ))
    })?;
    let size = file
        .metadata()
        .map_err(|err| {
            Error::Invalid(format!(
                "failed to inspect decrypted macOS package: {}",
                go_path_error("stat", path, &err)
            ))
        })?
        .len();
    if size == 0 {
        return Err(Error::Invalid("decrypted package is empty".to_owned()));
    }
    let archive = xar::Reader::new(file, size)
        .map_err(|err| Error::Invalid(format!("failed to parse XAR archive: {err}")))?;
    if archive.files.is_empty() {
        return Err(Error::Invalid(
            "XAR archive does not contain files".to_owned(),
        ));
    }
    for entry in archive.files.values() {
        if !archive.verify_checksum(entry) {
            return Err(Error::Invalid(format!(
                "XAR entry {} failed checksum validation",
                serde_json::to_string(&entry.name).unwrap_or_default()
            )));
        }
    }
    Ok(())
}

/// Go's `macDPInfo`: the one `dpInfo` value the sinfs carry.
pub(crate) fn mac_dp_info(sinfs: &[Sinf]) -> Result<Vec<u8>> {
    let mut dp_info: Option<&[u8]> = None;
    for sinf in sinfs.iter().filter(|sinf| !sinf.dp_info.is_empty()) {
        if dp_info.is_some_and(|value| value != sinf.dp_info) {
            return Err(Error::Invalid(
                "download response contains conflicting dpInfo values".to_owned(),
            ));
        }
        dp_info = Some(&sinf.dp_info);
    }
    dp_info
        .map(<[u8]>::to_vec)
        .ok_or_else(|| Error::Invalid("download response does not contain dpInfo".to_owned()))
}

/// Go's `cleanupMacPaths`: removes each path, ignoring missing ones.
fn cleanup_paths(fs: &dyn FileSystem, paths: &[PathBuf]) -> Result<()> {
    let errors: Vec<Error> = paths
        .iter()
        .filter_map(|path| match fs.remove(path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Some(Error::Invalid(format!(
                "failed to remove {}: {err}",
                path.display()
            ))),
            _ => None,
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Joined(errors))
    }
}

/// Go's `backupMacFile`: moves an existing destination aside, returning the backup path.
fn backup_file(fs: &dyn FileSystem, path: &Path) -> Result<Option<PathBuf>> {
    match std::fs::metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(Error::Invalid(format!(
                "failed to inspect destination package: {}",
                go_path_error("stat", path, &err)
            )));
        }
    }
    let backup = backup_path(path)?;
    fs.rename(path, &backup)
        .map_err(|err| Error::Invalid(format!("failed to back up destination package: {err}")))?;
    Ok(Some(backup))
}

/// Go's `macBackupPath`: the first of `.ipatool-backup`, `.ipatool-backup.1`, ... that
/// does not exist, so an unrelated backup is never replaced.
fn backup_path(path: &Path) -> Result<PathBuf> {
    for index in 0u64.. {
        let candidate = if index == 0 {
            with_suffix(path, BACKUP_SUFFIX)
        } else {
            with_suffix(path, &format!("{BACKUP_SUFFIX}.{index}"))
        };
        match std::fs::metadata(&candidate) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Err(err) => {
                return Err(Error::Invalid(format!(
                    "failed to inspect package backup path: {}",
                    go_path_error("stat", &candidate, &err)
                )));
            }
        }
    }
    Err(Error::Invalid(
        "failed to inspect package backup path".to_owned(),
    ))
}

/// Go's `restoreMacPackage`.
fn restore(fs: &dyn FileSystem, destination: &Path, backup: Option<&Path>) -> Result<()> {
    let mut errors = Vec::new();
    if let Err(err) = fs.remove(destination)
        && err.kind() != io::ErrorKind::NotFound
    {
        errors.push(Error::Invalid(format!(
            "failed to remove partially published package: {err}"
        )));
    }
    if let Some(backup) = backup
        && let Err(err) = fs.rename(backup, destination)
    {
        errors.push(Error::Invalid(format!("failed to restore package: {err}")));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Joined(errors))
    }
}

/// Go's `storeAgentDecrypter`.
struct StoreAgentDecrypter(StoreAgent);

/// Go's `defaultMacPackageDecrypterFactory`.
fn default_decrypter_factory(
    hardware_id: &[u8],
    dp_info: &[u8],
) -> Result<Box<dyn MacPackageDecrypter>> {
    let bundle = ipatool_sap::assets::load()
        .map_err(|err| Error::from(err).context("failed to load SAP assets"))?;
    let agent = StoreAgent::open(&bundle, hardware_id, dp_info)
        .map_err(|err| Error::from(err).context("failed to open StoreAgent"))?;
    Ok(Box::new(StoreAgentDecrypter(agent)))
}

impl MacPackageDecrypter for StoreAgentDecrypter {
    fn decrypt(&mut self, mut dst: &mut dyn Write, mut src: &mut dyn Read) -> Result<u64> {
        self.0
            .decrypt(&mut dst, &mut src)
            .map_err(|err| Error::from(err).context("failed to decrypt with StoreAgent"))
    }

    fn close(&mut self) -> Result<()> {
        self.0
            .close()
            .map_err(|err| Error::from(err).context("failed to close StoreAgent"))
    }
}

#[cfg(test)]
pub(crate) mod tests;
