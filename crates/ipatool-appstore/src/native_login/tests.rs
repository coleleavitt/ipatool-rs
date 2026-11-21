//! The native login flow against a fake backend (Go `appstore_native_login_test.go` cases).

use std::cell::RefCell;
use std::collections::VecDeque;

use ipatool_keychain::Config;
use tempfile::TempDir;

use super::*;
use crate::{Account, MachineAction};

const MAC: &str = "00:16:3e:12:34:56";
const GUID: &str = "00163E123456";
const ENDPOINT: &str = "https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate";

type Proof = Result<(BTreeMap<String, String>, Vec<u8>)>;

#[derive(Default)]
struct Fake {
    calls: RefCell<Vec<String>>,
    store_amd: RefCell<VecDeque<Proof>>,
    anisette: RefCell<Option<Result<(HeaderMap, Vec<u8>)>>>,
    logins: RefCell<VecDeque<Result<LoginOutput>>>,
    login_headers: RefCell<Vec<HeaderMap>>,
    provision: RefCell<Vec<(Vec<u8>, String, String, HeaderMap)>>,
}

impl Fake {
    fn call(&self, call: impl Into<String>) {
        self.calls.borrow_mut().push(call.into());
    }
}

#[allow(clippy::unused_async_trait_impl, reason = "the fake answers at once")]
impl NativeBackend for Fake {
    fn mac_address(&self) -> Result<String> {
        Ok(MAC.to_owned())
    }

    async fn auth_endpoint(&self, guid: &str) -> Result<String> {
        assert_eq!(guid, GUID);
        Ok(ENDPOINT.to_owned())
    }

    fn store_amd(
        &self,
        state: &[u8],
        endpoint: &str,
        hardware_id: &[u8],
    ) -> Result<(BTreeMap<String, String>, Vec<u8>)> {
        assert_eq!(endpoint, ENDPOINT);
        assert_eq!(hardware_id, [0x00, 0x16, 0x3e, 0x12, 0x34, 0x56]);
        self.call(format!("store_amd {}", String::from_utf8_lossy(state)));
        self.store_amd
            .borrow_mut()
            .pop_front()
            .expect("store_amd result")
    }

    async fn anisette(&self, state: &[u8]) -> Result<(HeaderMap, Vec<u8>)> {
        self.call(format!("anisette {}", String::from_utf8_lossy(state)));
        self.anisette.borrow_mut().take().expect("anisette result")
    }

    async fn kbsync(&self, _hardware_id: &[u8], dsid: u64) -> Result<Vec<u8>> {
        assert_eq!(dsid, u64::MAX - 1);
        self.call("kbsync");
        Ok(b"kb".to_vec())
    }

    async fn provision(
        &self,
        state: &[u8],
        challenge: &str,
        kbsync: &str,
        _hardware_id: &[u8],
        identity: &HeaderMap,
    ) -> Result<Vec<u8>> {
        self.call("provision");
        self.provision.borrow_mut().push((
            state.to_vec(),
            challenge.to_owned(),
            kbsync.to_owned(),
            identity.clone(),
        ));
        Ok(b"provisioned".to_vec())
    }

    async fn legacy_login(&self, input: &LoginInput, headers: &HeaderMap) -> Result<LoginOutput> {
        assert_eq!(input.email, "user@example.com");
        self.call("login");
        self.login_headers.borrow_mut().push(headers.clone());
        self.logins.borrow_mut().pop_front().expect("login result")
    }
}

fn keychain(dir: &TempDir) -> Keychain {
    Keychain::open_file(Config {
        service_name: "ipatool-test.service".to_owned(),
        id: None,
        file_dir: dir.path().to_owned(),
        passphrase: Box::new(|_| Ok("test-passphrase".to_owned())),
    })
}

fn input() -> LoginInput {
    LoginInput {
        email: "user@example.com".to_owned(),
        password: "secret".to_owned(),
        auth_code: String::new(),
    }
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "scripted login results are Results"
)]
fn success() -> Result<LoginOutput> {
    Ok(LoginOutput {
        account: Account {
            name: "Test User".to_owned(),
            ..Account::default()
        },
    })
}

fn action(name: &str, data: &str) -> Result<LoginOutput> {
    Err(Error::MachineAction(MachineAction {
        action: name.to_owned(),
        data: data.to_owned(),
    }))
}

fn anisette_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in MACHINE_HEADERS {
        headers.insert(name, HeaderValue::from_static("md"));
    }
    headers.insert("X-Mme-Device-Id", HeaderValue::from_static("device"));
    headers
}

fn proof(value: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("X-Apple-AMD".to_owned(), value.to_owned()),
        ("X-Apple-AMD-M".to_owned(), "m".to_owned()),
    ])
}

async fn stored(keychain: &Keychain) -> String {
    String::from_utf8(keychain.get(keys::ANISETTE).await.unwrap()).unwrap()
}

#[tokio::test]
async fn reuses_a_provisioned_store_amd_state() {
    let dir = TempDir::new().unwrap();
    let keychain = keychain(&dir);
    keychain.set(keys::ANISETTE, b"saved").await.unwrap();
    let fake = Fake::default();
    fake.store_amd
        .borrow_mut()
        .push_back(Ok((proof("amd"), b"next".to_vec())));
    fake.logins.borrow_mut().push_back(success());

    let output = native_login(&fake, &keychain, &input()).await.unwrap();

    assert_eq!(output.account.name, "Test User");
    assert_eq!(fake.calls.into_inner(), ["store_amd saved", "login"]);
    assert_eq!(fake.login_headers.borrow()[0]["X-Apple-AMD"], "amd");
    assert_eq!(stored(&keychain).await, "next");
}

#[tokio::test]
async fn provisions_anisette_without_a_saved_state() {
    let dir = TempDir::new().unwrap();
    let keychain = keychain(&dir);
    let fake = Fake::default();
    *fake.anisette.borrow_mut() = Some(Ok((anisette_headers(), b"machine".to_vec())));
    fake.logins.borrow_mut().push_back(success());

    native_login(&fake, &keychain, &input()).await.unwrap();

    assert_eq!(fake.calls.into_inner(), ["anisette ", "login"]);
    assert_eq!(fake.login_headers.into_inner(), [anisette_headers()]);
    assert_eq!(stored(&keychain).await, "machine");
}

#[tokio::test]
async fn falls_back_to_anisette_when_the_store_amd_proof_fails() {
    let dir = TempDir::new().unwrap();
    let keychain = keychain(&dir);
    keychain.set(keys::ANISETTE, b"saved").await.unwrap();
    let fake = Fake::default();
    fake.store_amd
        .borrow_mut()
        .push_back(Err(Error::NotPorted("the Store AMD framework")));
    *fake.anisette.borrow_mut() = Some(Ok((anisette_headers(), b"machine".to_vec())));
    fake.logins.borrow_mut().push_back(success());

    native_login(&fake, &keychain, &input()).await.unwrap();

    assert_eq!(
        fake.calls.into_inner(),
        ["store_amd saved", "anisette saved", "login"]
    );
}

#[tokio::test]
async fn retries_without_machine_headers_after_rp() {
    let dir = TempDir::new().unwrap();
    let keychain = keychain(&dir);
    let fake = Fake::default();
    *fake.anisette.borrow_mut() = Some(Ok((anisette_headers(), b"machine".to_vec())));
    fake.logins
        .borrow_mut()
        .extend([action("RP", ""), success()]);

    native_login(&fake, &keychain, &input()).await.unwrap();

    let headers = fake.login_headers.into_inner();
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0], anisette_headers());
    for name in MACHINE_HEADERS {
        assert!(!headers[1].contains_key(name), "{name} kept");
    }
    assert_eq!(headers[1]["X-Mme-Device-Id"], "device");
}

#[tokio::test]
async fn completes_store_provisioning_after_sp() {
    let dir = TempDir::new().unwrap();
    let keychain = keychain(&dir);
    let fake = Fake::default();
    *fake.anisette.borrow_mut() = Some(Ok((anisette_headers(), b"machine".to_vec())));
    fake.store_amd
        .borrow_mut()
        .push_back(Ok((proof("store"), b"store-state".to_vec())));
    fake.logins
        .borrow_mut()
        .extend([action("RP", ""), action("SP", "challenge"), success()]);

    native_login(&fake, &keychain, &input()).await.unwrap();

    assert_eq!(
        fake.calls.into_inner(),
        [
            "anisette ",
            "login",
            "login",
            "kbsync",
            "provision",
            "store_amd provisioned",
            "login"
        ]
    );
    let provision = fake.provision.into_inner();
    assert_eq!(provision[0].0, b"machine");
    assert_eq!(provision[0].1, "challenge");
    assert_eq!(provision[0].2, STANDARD.encode(b"kb"));
    // Go provisions with the original anisette headers, before the RP removal.
    assert_eq!(provision[0].3, anisette_headers());
    assert_eq!(fake.login_headers.borrow()[2]["X-Apple-AMD"], "store");
    assert_eq!(stored(&keychain).await, "store-state");
}

#[tokio::test]
async fn reports_errors_with_go_context() {
    let dir = TempDir::new().unwrap();
    let keychain = keychain(&dir);
    let fake = Fake::default();
    *fake.anisette.borrow_mut() = Some(Err(Error::Invalid("offline".to_owned())));
    let error = native_login(&fake, &keychain, &input()).await.unwrap_err();
    assert_eq!(error.to_string(), "prepare Apple authentication: offline");

    let fake = Fake::default();
    *fake.anisette.borrow_mut() = Some(Ok((anisette_headers(), b"machine".to_vec())));
    fake.store_amd
        .borrow_mut()
        .push_back(Err(Error::NotPorted("the Store AMD framework")));
    fake.logins
        .borrow_mut()
        .extend([action("RP", ""), action("SP", "challenge")]);
    let error = native_login(&fake, &keychain, &input()).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "prepare Store AMD proof: the Store AMD framework is not ported yet"
    );
}

#[tokio::test]
async fn returns_other_login_results_unchanged() {
    let dir = TempDir::new().unwrap();
    let keychain = keychain(&dir);
    let fake = Fake::default();
    *fake.anisette.borrow_mut() = Some(Ok((anisette_headers(), b"machine".to_vec())));
    fake.logins
        .borrow_mut()
        .push_back(Err(Error::AuthCodeRequired));
    let error = native_login(&fake, &keychain, &input()).await.unwrap_err();
    assert!(error.is_auth_code_required());

    let dir = TempDir::new().unwrap();
    let keychain = self::keychain(&dir);
    let fake = Fake::default();
    *fake.anisette.borrow_mut() = Some(Ok((anisette_headers(), b"machine".to_vec())));
    fake.logins
        .borrow_mut()
        .extend([action("RP", ""), action("XX", "")]);
    let error = native_login(&fake, &keychain, &input()).await.unwrap_err();
    assert_eq!(error.machine_action().unwrap().action, "XX");
}

#[tokio::test]
async fn skips_writing_an_unchanged_state() {
    let dir = TempDir::new().unwrap();
    let blocker = dir.path().join("not-a-directory");
    tokio::fs::write(&blocker, b"").await.unwrap();
    // Every write to this keychain fails, so only skipped writes succeed.
    let keychain = Keychain::open_file(Config {
        service_name: "ipatool-test.service".to_owned(),
        id: None,
        file_dir: blocker,
        passphrase: Box::new(|_| Ok("test-passphrase".to_owned())),
    });
    let mut state = State {
        keychain: &keychain,
        data: b"same".to_vec(),
    };
    state.save(b"same".to_vec(), "save").await.unwrap();
    let error = state.save(b"new".to_vec(), "save").await.unwrap_err();
    assert!(error.to_string().starts_with("save: "), "{error}");
    assert_eq!(state.data, b"same");
}
