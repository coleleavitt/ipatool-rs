use std::sync::Mutex;

use super::*;
use crate::xar::tests::{test_archive, test_xar};

fn io_error(err: &io::Error) -> Error {
    Error::Invalid(err.to_string())
}

const ENCRYPTED: &[u8] = b"encrypted package";
const DP_INFO: &[u8] = b"dp info";
const HARDWARE_ID: &[u8] = b"hardware";

#[derive(Default)]
struct FakeState {
    output: Vec<u8>,
    input: Vec<u8>,
    err: Option<String>,
    closed: bool,
    factory_calls: usize,
}

/// Go's `fakeMacPackageDecrypter`.
struct FakeDecrypter(Arc<Mutex<FakeState>>);

impl MacPackageDecrypter for FakeDecrypter {
    fn decrypt(&mut self, dst: &mut dyn Write, src: &mut dyn Read) -> Result<u64> {
        let mut input = Vec::new();
        src.read_to_end(&mut input).map_err(|err| io_error(&err))?;
        let mut state = self.0.lock().expect("fake state");
        state.input = input;
        if let Some(err) = &state.err {
            return Err(Error::Invalid(err.clone()));
        }
        dst.write_all(&state.output).map_err(|err| io_error(&err))?;
        Ok(state.output.len() as u64)
    }

    fn close(&mut self) -> Result<()> {
        self.0.lock().expect("fake state").closed = true;
        Ok(())
    }
}

/// Go's `macDownloadFailureOS`.
#[derive(Default)]
struct FailureFs {
    rename_error_old: Option<PathBuf>,
    remove_error_path: Option<PathBuf>,
}

impl FileSystem for FailureFs {
    fn remove(&self, path: &Path) -> io::Result<()> {
        if self.remove_error_path.as_deref() == Some(path) {
            return Err(io::Error::other("remove failed"));
        }
        OsFileSystem.remove(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.rename_error_old.as_deref() == Some(from) {
            return Err(io::Error::other("rename failed"));
        }
        OsFileSystem.rename(from, to)
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    destination: PathBuf,
    state: Arc<Mutex<FakeState>>,
    decrypted: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temp dir");
        let destination = directory.path().join("app.pkg");
        let decrypted = test_xar(b"decrypted payload", false);
        let state = Arc::new(Mutex::new(FakeState {
            output: decrypted.clone(),
            ..FakeState::default()
        }));
        Self {
            _directory: directory,
            destination,
            state,
            decrypted,
        }
    }

    fn job(&self, fs: impl FileSystem + 'static) -> MacPackageDownload {
        let state = Arc::clone(&self.state);
        MacPackageDownload {
            fs: Arc::new(fs),
            factory: Arc::new(move |hardware_id, dp_info| {
                assert_eq!(hardware_id, HARDWARE_ID);
                assert_eq!(dp_info, DP_INFO);
                state.lock().expect("fake state").factory_calls += 1;
                Ok(Box::new(FakeDecrypter(Arc::clone(&state))))
            }),
        }
    }

    fn item(dp_info: &[u8]) -> DownloadItem {
        DownloadItem {
            url: "https://example.test/app.pkg".to_owned(),
            sinfs: vec![Sinf {
                dp_info: dp_info.to_vec(),
                ..Sinf::default()
            }],
            ..DownloadItem::default()
        }
    }

    async fn download_with(&self, fs: impl FileSystem + 'static) -> Result<DownloadOutput> {
        self.job(fs)
            .run(
                &Self::item(DP_INFO),
                &self.destination,
                HARDWARE_ID,
                fake_download,
            )
            .await
    }

    async fn download(&self) -> Result<DownloadOutput> {
        self.download_with(OsFileSystem).await
    }

    fn path(&self, suffix: &str) -> PathBuf {
        with_suffix(&self.destination, suffix)
    }

    fn write(&self, suffix: &str, data: &[u8]) {
        std::fs::write(self.path(suffix), data).expect("write fixture");
    }

    fn read(&self, suffix: &str) -> Vec<u8> {
        std::fs::read(self.path(suffix)).expect("read fixture")
    }

    fn exists(&self, suffix: &str) -> bool {
        self.path(suffix).exists()
    }

    fn expect_no_staging(&self) {
        assert!(!self.exists(ENCRYPTED_STAGE_SUFFIX));
        assert!(!self.exists(DECRYPTED_STAGE_SUFFIX));
        assert!(!self.exists(BACKUP_SUFFIX));
    }

    fn output(&self) -> DownloadOutput {
        DownloadOutput {
            destination_path: self.destination.clone(),
            sinfs: Vec::new(),
        }
    }
}

async fn fake_download(url: &str, path: &Path) -> Result<()> {
    assert_eq!(url, "https://example.test/app.pkg");
    std::fs::write(path, ENCRYPTED).map_err(|err| io_error(&err))
}

fn message(result: Result<DownloadOutput>) -> String {
    result.expect_err("download error").to_string()
}

#[tokio::test]
async fn publishes_only_the_decrypted_validated_package() {
    let fixture = Fixture::new();
    let out = fixture.download().await.expect("download");
    assert_eq!(out, fixture.output());
    assert_eq!(fixture.read(""), fixture.decrypted);
    let state = fixture.state.lock().expect("fake state");
    assert_eq!(state.input, ENCRYPTED);
    assert!(state.closed);
    assert!(!fixture.exists(DP_INFO_SUFFIX));
    assert!(!fixture.exists(HW_INFO_SUFFIX));
    fixture.expect_no_staging();
}

#[tokio::test]
async fn replaces_an_existing_package_and_removes_legacy_sidecars_after_success() {
    let fixture = Fixture::new();
    fixture.write("", b"old package");
    fixture.write(DP_INFO_SUFFIX, b"old dp");
    fixture.write(HW_INFO_SUFFIX, b"old hw");
    fixture.download().await.expect("download");
    assert_eq!(fixture.read(""), fixture.decrypted);
    assert!(!fixture.exists(DP_INFO_SUFFIX));
    assert!(!fixture.exists(HW_INFO_SUFFIX));
    fixture.expect_no_staging();
}

#[tokio::test]
async fn succeeds_when_legacy_sidecar_cleanup_fails_after_publication() {
    let fixture = Fixture::new();
    fixture.write(DP_INFO_SUFFIX, b"old dp");
    let fs = FailureFs {
        remove_error_path: Some(fixture.path(DP_INFO_SUFFIX)),
        ..FailureFs::default()
    };
    let out = fixture.download_with(fs).await.expect("download");
    assert_eq!(out, fixture.output());
    assert_eq!(fixture.read(""), fixture.decrypted);
    assert_eq!(fixture.read(DP_INFO_SUFFIX), b"old dp");
    fixture.expect_no_staging();
}

#[tokio::test]
async fn preserves_the_existing_package_when_decryption_fails() {
    let fixture = Fixture::new();
    fixture.write("", b"old package");
    fixture.state.lock().expect("fake state").err = Some("decrypt failed".to_owned());
    assert_eq!(
        message(fixture.download().await),
        "failed to decrypt macOS package: decrypt failed"
    );
    assert_eq!(fixture.read(""), b"old package");
    assert!(fixture.state.lock().expect("fake state").closed);
    fixture.expect_no_staging();
}

/// Go cancels the request context; here the download future is dropped mid-transfer.
#[tokio::test]
async fn cancels_the_package_request_and_cleans_staging() {
    let fixture = Fixture::new();
    let job = fixture.job(OsFileSystem);
    let item = Fixture::item(DP_INFO);
    let (started, wait) = tokio::sync::oneshot::channel();
    let started = Mutex::new(Some(started));
    let stalled = async |_: &str, path: &Path| -> Result<()> {
        std::fs::write(path, b"partial").map_err(|err| io_error(&err))?;
        if let Some(started) = started.lock().expect("signal").take() {
            let _ = started.send(());
        }
        std::future::pending().await
    };
    tokio::select! {
        _ = job.run(&item, &fixture.destination, HARDWARE_ID, stalled) => panic!("download finished"),
        _ = wait => {}
    }
    assert!(!fixture.destination.exists());
    fixture.expect_no_staging();
    assert_eq!(fixture.state.lock().expect("fake state").factory_calls, 0);
}

#[tokio::test]
async fn preserves_the_existing_package_when_decrypted_output_is_not_xar() {
    let fixture = Fixture::new();
    fixture.write("", b"old package");
    fixture.state.lock().expect("fake state").output = b"not xar".to_vec();
    assert_eq!(
        message(fixture.download().await),
        "failed to validate decrypted macOS package: failed to parse XAR archive: EOF"
    );
    assert_eq!(fixture.read(""), b"old package");
    fixture.expect_no_staging();
}

#[tokio::test]
async fn rejects_an_empty_decrypted_package() {
    let fixture = Fixture::new();
    fixture.state.lock().expect("fake state").output = Vec::new();
    assert_eq!(
        message(fixture.download().await),
        "failed to validate decrypted macOS package: decrypted package is empty"
    );
    assert!(!fixture.destination.exists());
    fixture.expect_no_staging();
}

#[tokio::test]
async fn rejects_a_xar_without_files() {
    let fixture = Fixture::new();
    fixture.write("", b"old package");
    fixture.state.lock().expect("fake state").output = test_archive("", b"");
    assert!(message(fixture.download().await).contains("does not contain files"));
    assert_eq!(fixture.read(""), b"old package");
    fixture.expect_no_staging();
}

#[tokio::test]
async fn rejects_a_xar_entry_with_an_invalid_checksum() {
    let fixture = Fixture::new();
    fixture.write("", b"old package");
    fixture.state.lock().expect("fake state").output = test_xar(b"decrypted payload", true);
    assert_eq!(
        message(fixture.download().await),
        "failed to validate decrypted macOS package: XAR entry \"Payload\" failed checksum validation"
    );
    assert_eq!(fixture.read(""), b"old package");
    fixture.expect_no_staging();
}

#[tokio::test]
async fn restores_the_existing_package_when_publication_fails() {
    let fixture = Fixture::new();
    fixture.write("", b"old package");
    let fs = FailureFs {
        rename_error_old: Some(fixture.path(DECRYPTED_STAGE_SUFFIX)),
        ..FailureFs::default()
    };
    assert_eq!(
        message(fixture.download_with(fs).await),
        "failed to publish package: rename failed"
    );
    assert_eq!(fixture.read(""), b"old package");
    fixture.expect_no_staging();
}

#[tokio::test]
async fn reports_a_failed_rollback() {
    let fixture = Fixture::new();
    fixture.write("", b"old package");
    let backup = fixture.path(BACKUP_SUFFIX);
    let fs = FailureFs {
        rename_error_old: Some(fixture.path(DECRYPTED_STAGE_SUFFIX)),
        remove_error_path: Some(fixture.destination.clone()),
    };
    let message = message(fixture.download_with(fs).await);
    assert_eq!(
        message,
        "failed to publish package: rename failed\nfailed to restore existing macOS download: failed to remove partially published package: remove failed"
    );
    // Nothing was published, so the backup went back in place.
    assert_eq!(fixture.read(""), b"old package");
    assert!(!backup.exists());
}

#[tokio::test]
async fn does_not_replace_an_unrelated_existing_backup() {
    let fixture = Fixture::new();
    fixture.write("", b"old package");
    fixture.write(BACKUP_SUFFIX, b"unrelated backup");
    fixture.download().await.expect("download");
    assert_eq!(fixture.read(""), fixture.decrypted);
    assert_eq!(fixture.read(BACKUP_SUFFIX), b"unrelated backup");
    assert!(!fixture.exists(&format!("{BACKUP_SUFFIX}.1")));
}

#[tokio::test]
async fn rejects_a_download_response_without_dp_info_before_downloading() {
    let fixture = Fixture::new();
    let downloaded = Mutex::new(false);
    let result = fixture
        .job(OsFileSystem)
        .run(
            &DownloadItem::default(),
            &fixture.destination,
            HARDWARE_ID,
            async |_: &str, _: &Path| {
                *downloaded.lock().expect("flag") = true;
                Ok(())
            },
        )
        .await;
    assert_eq!(message(result), "download response does not contain dpInfo");
    assert_eq!(fixture.state.lock().expect("fake state").factory_calls, 0);
    assert!(!*downloaded.lock().expect("flag"));
    assert!(!fixture.destination.exists());
}

#[tokio::test]
async fn reports_download_failures_and_cleans_staging() {
    let fixture = Fixture::new();
    let result = fixture
        .job(OsFileSystem)
        .run(
            &Fixture::item(DP_INFO),
            &fixture.destination,
            HARDWARE_ID,
            async |_: &str, path: &Path| {
                std::fs::write(path, b"partial").map_err(|err| io_error(&err))?;
                Err(Error::Invalid("connection reset".to_owned()))
            },
        )
        .await;
    assert_eq!(message(result), "failed to download file: connection reset");
    fixture.expect_no_staging();
}

#[test]
fn rejects_conflicting_dp_info_values() {
    let sinfs = [b"one", b"two"].map(|dp_info| Sinf {
        dp_info: dp_info.to_vec(),
        ..Sinf::default()
    });
    assert_eq!(
        mac_dp_info(&sinfs).expect_err("conflict").to_string(),
        "download response contains conflicting dpInfo values"
    );
    let same = [b"one", b"one"].map(|dp_info| Sinf {
        dp_info: dp_info.to_vec(),
        ..Sinf::default()
    });
    assert_eq!(mac_dp_info(&same).expect("dpInfo"), b"one");
}

/// The OS text differs per platform (Go prints Windows' own text there too).
#[cfg(unix)]
#[test]
fn os_errors_use_go_text() {
    let missing = Path::new("/nonexistent/ipatool");
    assert_eq!(
        OsFileSystem
            .remove(missing)
            .expect_err("missing")
            .to_string(),
        "remove /nonexistent/ipatool: no such file or directory"
    );
    assert_eq!(
        OsFileSystem
            .rename(missing, Path::new("/nonexistent/b"))
            .expect_err("missing")
            .to_string(),
        "rename /nonexistent/ipatool /nonexistent/b: no such file or directory"
    );
}
