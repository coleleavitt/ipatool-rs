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
        id: None,
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
        id: None,
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

/// `Config::id` keeps a second login beside the Go entry (majd/ipatool#235): the key
/// is `<key>/<id>`, and the Go key is neither read nor changed.
#[tokio::test]
async fn an_id_selects_its_own_entries() {
    let dir = tempfile::tempdir().unwrap();
    let default = keychain(dir.path());
    let work = Keychain::open_file(Config {
        service_name: "ipatool-auth.service".to_owned(),
        id: Some("work@example.com".to_owned()),
        file_dir: dir.path().to_path_buf(),
        passphrase: Box::new(|_| Ok("test-passphrase".to_owned())),
    });
    default.set("account", b"default").await.unwrap();
    assert!(work.get("account").await.unwrap_err().is_not_found());
    work.set("account", b"work").await.unwrap();
    assert_eq!(default.get("account").await.unwrap(), b"default");
    assert_eq!(work.get("account").await.unwrap(), b"work");
    assert_eq!(
        default.get("account/work@example.com").await.unwrap(),
        b"work"
    );
    assert!(dir.path().join("account%2Fwork@example.com").is_file());
    work.remove("account").await.unwrap();
    assert_eq!(default.get("account").await.unwrap(), b"default");
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

    // An entry kept under an ID is an ordinary Go entry named `<key>/<id>`.
    let work = Keychain::open_file(Config {
        service_name: "ipatool-auth.service".to_owned(),
        id: Some("work".to_owned()),
        file_dir: dir.path().to_path_buf(),
        passphrase: Box::new(|_| Ok("test-passphrase".to_owned())),
    });
    work.set("account", b"work-data").await.unwrap();
    let out = Command::new(&helper)
        .args([dir.path().to_str().unwrap(), "get", "account/work"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        "work-data|ipatool-auth.service"
    );
}
