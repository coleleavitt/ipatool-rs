//! Cross-checks the file backend against Go's `byteness/keyring`. Needs a helper
//! binary built from `tests/go_helper` (see the AGENTS.md gate); skipped when
//! `IPATOOL_GO_KEYRING` is unset.

use std::path::Path;
use std::process::Command;

use ipatool_keychain::{Config, Keychain};

fn helper() -> Option<String> {
    std::env::var("IPATOOL_GO_KEYRING").ok()
}

fn keychain(dir: &Path) -> Keychain {
    Keychain::open_file(Config {
        service_name: "ipatool-auth.service".to_owned(),
        file_dir: dir.to_path_buf(),
        passphrase: Box::new(|_| Ok("test-passphrase".to_owned())),
    })
}

/// Runs set/get/remove against a throwaway collection in the live Secret Service:
/// `IPATOOL_TEST_SECRET_SERVICE=1`. The real `ipatool-auth.service` is not touched.
#[tokio::test]
async fn secret_service_round_trip() {
    if std::env::var("IPATOOL_TEST_SECRET_SERVICE").is_err() {
        return;
    }
    let ring = Keychain::open(Config {
        service_name: "ipatool-rust-test.service".to_owned(),
        file_dir: std::env::temp_dir(),
        passphrase: Box::new(|_| Err("unused".to_owned())),
    })
    .await;
    assert_eq!(ring.backend_name(), "secret-service");
    ring.set("account", b"one").await.unwrap();
    ring.set("account", b"two").await.unwrap();
    assert_eq!(ring.get("account").await.unwrap(), b"two");
    ring.remove("account").await.unwrap();
    let error = ring.get("account").await.unwrap_err();
    assert!(error.is_not_found());
    assert_eq!(
        error.to_string(),
        "failed to get item: The specified item could not be found in the keyring"
    );
}

#[tokio::test]
async fn reads_go_files_and_go_reads_ours() {
    let Some(helper) = helper() else {
        eprintln!("IPATOOL_GO_KEYRING unset; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ring = keychain(dir.path());

    let status = Command::new(&helper)
        .args([
            dir.path().to_str().unwrap(),
            "set",
            "from-go",
            r#"{"email":"a@b"}"#,
        ])
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(ring.get("from-go").await.unwrap(), br#"{"email":"a@b"}"#);

    ring.set("from/rust", b"rust-data").await.unwrap();
    let out = Command::new(&helper)
        .args([dir.path().to_str().unwrap(), "get", "from/rust"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        "rust-data|ipatool-auth.service"
    );
}
