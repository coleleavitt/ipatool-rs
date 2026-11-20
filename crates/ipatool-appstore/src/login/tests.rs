//! Port of Go `appstore_login_test.go`, `authentication_redirect_test.go` and
//! `authentication_retry_test.go`, with a fake HTTP layer, machine, bag and signer.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ipatool_keychain::Config;
use reqwest::header::HeaderValue;

use super::*;
use crate::{TransportError, UnexpectedResponseError};

const TEST_PASSWORD: &str = "test-password";
const TEST_EMAIL: &str = "test-email";
const TEST_FIRST_NAME: &str = "test-first-name";
const TEST_LAST_NAME: &str = "test-last-name";
const TEST_POD: &str = "42";
const AUTH: &str = "https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate";
const AUTH_PATH: &str = "/WebObjects/MZFinance.woa/wa/authenticate";

fn valid_sap_config() -> SapConfig {
    SapConfig {
        auth_endpoint: format!("{AUTH}/"),
        setup_url: "https://fpinit.example.com/v1/signSapSetup/legacy".to_owned(),
        certificate_url: "https://static.example.com/sap/setupCert.plist".to_owned(),
        version: 200,
    }
}

#[derive(Default)]
struct SignerState {
    close_calls: Cell<usize>,
    close_error: RefCell<Option<String>>,
}

struct FakeSigner(Rc<SignerState>);

impl ActionSigner for FakeSigner {
    fn sign(&mut self, input: &[u8]) -> ipatool_sap::Result<Vec<u8>> {
        Ok(input.to_vec())
    }

    fn close(&mut self) -> ipatool_sap::Result<()> {
        self.0.close_calls.set(self.0.close_calls.get() + 1);
        match self.0.close_error.borrow().as_deref() {
            Some(message) => Err(ipatool_sap::Error::msg(message)),
            None => Ok(()),
        }
    }
}

type Response = Result<AuthResult<LoginResult>>;
type Responder = Box<dyn FnMut(usize, &AuthRequest) -> Response>;

struct Fake {
    mac_address: std::result::Result<String, String>,
    signer: Rc<SignerState>,
    signer_error: Option<String>,
    responder: RefCell<Responder>,
    calls: Cell<usize>,
    waits: RefCell<Vec<Duration>>,
    sleep_forbidden: bool,
}

impl Fake {
    fn new(responder: impl FnMut(usize, &AuthRequest) -> Response + 'static) -> Self {
        Self {
            mac_address: Ok("00:00:00:00:00:00".to_owned()),
            signer: Rc::default(),
            signer_error: None,
            responder: RefCell::new(Box::new(responder)),
            calls: Cell::new(0),
            waits: RefCell::default(),
            sleep_forbidden: false,
        }
    }

    /// A fake whose every response is `response`.
    fn always(response: impl Fn() -> Response + 'static) -> Self {
        Self::new(move |_, _| response())
    }

    fn waits(&self) -> Vec<Duration> {
        self.waits.borrow().clone()
    }
}

#[allow(clippy::unused_async_trait_impl, reason = "the fake answers at once")]
impl LoginBackend for Fake {
    type Signer = FakeSigner;

    fn mac_address(&self) -> Result<String> {
        self.mac_address.clone().map_err(Error::Invalid)
    }

    async fn sap_config(&self, _: &Client, guid: &str) -> Result<SapConfig> {
        assert_eq!(guid, "000000000000");
        Ok(valid_sap_config())
    }

    async fn create_signer(
        &self,
        _: &Client,
        config: &SapConfig,
        machine_id: &[u8],
    ) -> Result<FakeSigner> {
        assert_eq!(*config, valid_sap_config());
        assert_eq!(machine_id, [0; 6]);
        match &self.signer_error {
            Some(message) => Err(Error::Invalid(message.clone())),
            None => Ok(FakeSigner(Rc::clone(&self.signer))),
        }
    }

    async fn send(&self, _: &Client, request: &AuthRequest, signer: &mut FakeSigner) -> Response {
        assert!(
            Rc::ptr_eq(&signer.0, &self.signer),
            "the login signer signs every request"
        );
        let call = self.calls.get();
        self.calls.set(call + 1);
        (self.responder.borrow_mut())(call, request)
    }

    async fn sleep(&self, delay: Duration) {
        assert!(!self.sleep_forbidden, "must not sleep");
        self.waits.borrow_mut().push(delay);
    }
}

struct Harness {
    client: Client,
    keychain: Keychain,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::open_file(Config {
            service_name: "ipatool-test.service".to_owned(),
            file_dir: dir.path().to_owned(),
            passphrase: Box::new(|_| Ok("test-passphrase".to_owned())),
        });
        let client = Client::new().unwrap();
        client.cache_storefronts(&[
            ("143441", "US"),
            ("143443", "DE"),
            ("143444", "GB"),
            ("143465", "CN"),
            ("143467", "IN"),
            ("143469", "RU"),
        ]);
        Self {
            client,
            keychain,
            _dir: dir,
        }
    }

    fn login<'a>(&'a self, backend: &'a Fake) -> Login<'a, Fake> {
        Login {
            client: &self.client,
            backend,
            keychain: &self.keychain,
            authentication_headers: None,
        }
    }

    async fn saved_account(&self) -> Option<Account> {
        let data = self.keychain.get(keys::ACCOUNT).await.ok()?;
        Some(serde_json::from_slice(&data).unwrap())
    }
}

fn input(password: &str, auth_code: &str) -> LoginInput {
    LoginInput {
        email: String::new(),
        password: password.to_owned(),
        auth_code: auth_code.to_owned(),
    }
}

fn credentials<'a>(email: &'a str, auth_code: &'a str) -> Credentials<'a> {
    Credentials {
        email,
        password: "password",
        auth_code,
        guid: "guid",
    }
}

#[allow(clippy::unnecessary_wraps, reason = "builds a fake `send` answer")]
fn result(status: u16, headers: &[(&str, &str)], data: LoginResult) -> Response {
    Ok(AuthResult {
        status,
        headers: headers
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        data,
    })
}

fn failure(failure_type: &str, customer_message: &str) -> LoginResult {
    LoginResult {
        failure_type: failure_type.to_owned(),
        customer_message: customer_message.to_owned(),
        ..LoginResult::default()
    }
}

fn success() -> LoginResult {
    LoginResult {
        password_token: "token".to_owned(),
        directory_services_id: "123".to_owned(),
        ..LoginResult::default()
    }
}

fn unexpected(status: u16, retry_after: &str) -> Error {
    Error::UnexpectedResponse(UnexpectedResponseError {
        status,
        retry_after: retry_after.to_owned(),
        ..UnexpectedResponseError::default()
    })
}

fn transport(source: impl std::error::Error + Send + Sync + 'static, retry_after: &str) -> Error {
    Error::Transport(TransportError {
        context: "request failed",
        source: Box::new(source),
        retry_after: retry_after.to_owned(),
    })
}

fn complete() -> Response {
    Err(Error::Invalid("test complete".to_owned()))
}

fn payload<'a>(request: &'a AuthRequest, key: &str) -> &'a str {
    request.payload[key].as_string().unwrap()
}

#[tokio::test]
async fn rejects_malformed_2fa_codes_before_preparing_authentication() {
    let harness = Harness::new();
    let mut fake = Fake::always(|| unreachable!());
    fake.mac_address = Err("must not be read".to_owned());
    for code in [
        " \t\r\n",
        "12345",
        "1234567",
        "12345a",
        "１２３４５６",
        "\x1b[31m123456",
        "\x1b[200~123456",
        "\x1b[200~\x1b[201~",
        "123\x1b[200~456\x1b[201~",
    ] {
        let error = harness
            .login(&fake)
            .legacy_login(&input("", code))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "2FA code must contain exactly six digits",
            "{code:?}"
        );
    }
}

#[tokio::test]
async fn retries_the_same_request_until_it_succeeds() {
    for (apple_id, storefront) in [
        (TEST_EMAIL, None),
        ("+8613912345678", Some("143465")),
        ("9123456789", Some("143467")),
    ] {
        let harness = Harness::new();
        let fake = Fake::new(move |call, request| {
            assert_eq!(request.url, AUTH);
            assert_eq!(
                request
                    .headers
                    .get("X-Apple-Store-Front")
                    .map(String::as_str),
                storefront
            );
            assert_eq!(payload(request, "appleId"), apple_id);
            assert_eq!(payload(request, "attempt"), "1");
            match call {
                0 => Err(unexpected(204, "")),
                1 => Err(unexpected(404, "")),
                _ => result(200, &[], LoginResult::default()),
            }
        });
        let login = harness.login(&fake);
        let request = login.login_request(&credentials(apple_id, ""), AUTH, 1);
        let response = login
            .send_authentication_request(&request, &mut FakeSigner(Rc::clone(&fake.signer)))
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(fake.calls.get(), 3);
    }
}

#[tokio::test]
async fn bounds_retries_to_transient_statuses() {
    for (status, expected_calls) in [(204, 3), (503, 3), (403, 3), (401, 1)] {
        let harness = Harness::new();
        let fake = Fake::always(move || Err(unexpected(status, "")));
        let login = harness.login(&fake);
        let request = login.login_request(&credentials(TEST_EMAIL, ""), AUTH, 1);
        let error = login
            .send_authentication_request(&request, &mut FakeSigner(Rc::clone(&fake.signer)))
            .await
            .unwrap_err();
        assert_eq!(fake.calls.get(), expected_calls, "{status}");
        assert_eq!(unexpected_status(&error), Some(status));
        if expected_calls == 3 {
            let message = error.to_string();
            assert!(message.contains("after 3 attempts"), "{message}");
            assert!(
                message.contains(&format!("HTTP {status}, {status}, {status}")),
                "{message}"
            );
        }
    }
}

#[test]
fn does_not_expose_untrusted_dialog_actions() {
    for (kind, url) in [
        ("OpenURL", "https://example.com/reset"),
        ("OpenURL", "http://iforgot.apple.com/reset"),
        ("OpenURL", "https://user@iforgot.apple.com/reset"),
        ("OpenURL", "https://iforgot.apple.com:8443/reset"),
        ("OpenURL", "https://iforgot.apple.com/reset#token"),
        ("Buy", "https://iforgot.apple.com/reset"),
    ] {
        let result = LoginResult {
            dialog: LoginDialogResult {
                message: "Account action required".to_owned(),
                ok_button_string: "Continue".to_owned(),
                ok_button_action: LoginDialogActionResult {
                    kind: kind.to_owned(),
                    url: url.to_owned(),
                },
                ..LoginDialogResult::default()
            },
            ..LoginResult::default()
        };
        assert_eq!(
            authentication_dialog_error(&result).as_deref(),
            Some("Account action required"),
            "{url}"
        );
    }
}

#[tokio::test]
async fn reports_a_mac_address_failure() {
    let harness = Harness::new();
    let mut fake = Fake::always(|| unreachable!());
    fake.mac_address = Err("no adapter".to_owned());
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "failed to get mac address: no adapter");
}

#[tokio::test]
async fn normalizes_2fa_codes_without_changing_the_password() {
    const PASSWORD: &str = " \tpäss word\n";
    for (code, suffix) in [
        ("", ""),
        ("012345", "012345"),
        ("123 456", "123456"),
        ("\t123\u{a0}456\r\n", "123456"),
        ("\x1b[200~123456\x1b[201~", "123456"),
        (" \x1b[200~123 456\n\x1b[201~\r\n", "123456"),
    ] {
        let harness = Harness::new();
        let expected = format!("{PASSWORD}{suffix}");
        let fake = Fake::new(move |_, request| {
            assert_eq!(request.url, format!("{AUTH}/"));
            assert_eq!(payload(request, "password"), expected);
            complete()
        });
        let error = harness
            .login(&fake)
            .legacy_login(&input(PASSWORD, code))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("test complete"), "{error}");
    }
}

#[tokio::test]
async fn returns_a_wrapped_client_error_and_closes_the_signer() {
    let harness = Harness::new();
    let fake = Fake::new(|_, request| {
        assert_eq!(request.url, format!("{AUTH}/"));
        Err(Error::Invalid("client error".to_owned()))
    });
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "sign-in request failed: client error");
    assert_eq!(fake.signer.close_calls.get(), 1);
}

#[tokio::test]
async fn preserves_both_the_request_and_cleanup_errors() {
    let harness = Harness::new();
    let fake = Fake::always(|| Err(Error::Invalid("client error".to_owned())));
    *fake.signer.close_error.borrow_mut() = Some("cleanup failed".to_owned());
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sign-in request failed: client error\nfailed to close SAP action signer: cleanup failed"
    );
    assert_eq!(fake.signer.close_calls.get(), 1);
}

#[tokio::test]
async fn reports_a_signer_initialization_failure() {
    let harness = Harness::new();
    let mut fake = Fake::always(|| unreachable!());
    fake.signer_error = Some("signer error".to_owned());
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to initialize SAP action signer: signer error"
    );
}

#[tokio::test]
async fn retries_invalid_credentials_once_then_fails() {
    let harness = Harness::new();
    let fake = Fake::always(|| result(0, &[], failure(FAILURE_TYPE_INVALID_CREDENTIALS, "")));
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert_eq!(fake.calls.get(), 2);
    assert_eq!(
        error.to_string(),
        "something went wrong (failureType: -5000)"
    );
}

#[tokio::test]
async fn maps_failure_type_5005_to_an_invalid_auth_code() {
    let harness = Harness::new();
    let fake = Fake::always(|| result(0, &[], failure(FAILURE_TYPE_INVALID_AUTH_CODE, "")));
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, "123456"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidAuthCode(None)), "{error}");
    assert_eq!(error.to_string(), "invalid or expired 2FA code");
}

#[tokio::test]
async fn maps_invalid_credentials_with_a_code_to_an_invalid_auth_code() {
    let harness = Harness::new();
    let fake = Fake::new(|call, _| {
        let message = if call == 0 {
            ""
        } else {
            "Your Apple ID or password was entered incorrectly."
        };
        result(200, &[], failure(FAILURE_TYPE_INVALID_CREDENTIALS, message))
    });
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, "123456"))
        .await
        .unwrap_err();
    assert_eq!(fake.calls.get(), 2);
    assert!(error.is_invalid_auth_code(), "{error}");
}

#[tokio::test]
async fn returns_an_error_with_the_failure_type() {
    let harness = Harness::new();
    let fake = Fake::always(|| result(0, &[], failure("random-error", "")));
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("something went wrong (failureType: random-error)"),
        "{error}"
    );
}

#[tokio::test]
async fn reports_a_disabled_account() {
    let harness = Harness::new();
    let fake = Fake::always(|| result(0, &[], failure("", CUSTOMER_MESSAGE_ACCOUNT_DISABLED)));
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("account is disabled"), "{error}");
}

#[tokio::test]
async fn surfaces_the_dialog_and_trusted_recovery_url() {
    let harness = Harness::new();
    let fake = Fake::always(|| {
        result(
            200,
            &[],
            LoginResult {
                customer_message: CUSTOMER_MESSAGE_ACCOUNT_DISABLED.to_owned(),
                dialog: LoginDialogResult {
                    message: "This Apple Account has been locked for security reasons.".to_owned(),
                    explanation: "Reset your password to continue.".to_owned(),
                    ok_button_string: "Reset".to_owned(),
                    ok_button_action: LoginDialogActionResult {
                        kind: "OpenURL".to_owned(),
                        url: "https://iforgot.apple.com/cgi-bin/iforgot.cgi".to_owned(),
                    },
                    ..LoginDialogResult::default()
                },
                ..LoginResult::default()
            },
        )
    });
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "This Apple Account has been locked for security reasons.: Reset your password to continue. (Reset: https://iforgot.apple.com/cgi-bin/iforgot.cgi)"
    );
}

#[tokio::test]
async fn requires_a_2fa_code() {
    let harness = Harness::new();
    let fake = Fake::always(|| result(200, &[], failure("", CUSTOMER_MESSAGE_BAD_LOGIN)));
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::AuthCodeRequired), "{error}");

    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, "123456"))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("apple did not complete verification; try a fresh 2FA code"),
        "{error}"
    );
    assert!(error.is_invalid_auth_code());
    assert!(!error.is_auth_code_required());
}

#[tokio::test]
async fn follows_a_redirect_while_preserving_the_original_request_body() {
    const LOCATION: &str =
        "https://p42-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate";
    let harness = Harness::new();
    let fake = Fake::new(|call, request| {
        assert_eq!(payload(request, "attempt"), "1");
        if call == 0 {
            assert_eq!(request.url, format!("{AUTH}/"));
            return result(302, &[("Location", LOCATION)], LoginResult::default());
        }
        assert_eq!(request.url, LOCATION);
        complete()
    });
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sign-in at Store pod request failed: test complete"
    );
    assert_eq!(fake.calls.get(), 2);
}

#[tokio::test]
async fn rejects_a_redirect_outside_apple_authentication_pods() {
    let harness = Harness::new();
    let fake = Fake::always(|| {
        result(
            302,
            &[("Location", "https://example.com/steal")],
            LoginResult::default(),
        )
    });
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("invalid authentication redirect"),
        "{error}"
    );
}

fn valid_response() -> Response {
    result(
        200,
        &[
            (HTTP_HEADER_STORE_FRONT, "test-storefront"),
            (HTTP_HEADER_POD, TEST_POD),
        ],
        LoginResult {
            password_token: "test-password-token".to_owned(),
            directory_services_id: "directory-services-id".to_owned(),
            account: LoginAccountResult {
                email: TEST_EMAIL.to_owned(),
                address: LoginAddressResult {
                    first_name: TEST_FIRST_NAME.to_owned(),
                    last_name: TEST_LAST_NAME.to_owned(),
                },
            },
            ..LoginResult::default()
        },
    )
}

fn expected_account() -> Account {
    Account {
        name: format!("{TEST_FIRST_NAME} {TEST_LAST_NAME}"),
        email: TEST_EMAIL.to_owned(),
        password_token: "test-password-token".to_owned(),
        password: TEST_PASSWORD.to_owned(),
        directory_services_id: "directory-services-id".to_owned(),
        store_front: "test-storefront".to_owned(),
        pod: TEST_POD.to_owned(),
    }
}

#[tokio::test]
async fn saves_the_account_in_the_keychain() {
    let harness = Harness::new();
    let fake = Fake::always(valid_response);
    let output = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap();
    assert_eq!(output.account, expected_account());
    assert_eq!(harness.saved_account().await, Some(expected_account()));
}

#[tokio::test]
async fn saves_the_account_and_reports_a_signer_cleanup_error() {
    let harness = Harness::new();
    let fake = Fake::always(valid_response);
    *fake.signer.close_error.borrow_mut() = Some("cleanup failed".to_owned());
    let error = harness
        .login(&fake)
        .legacy_login(&input(TEST_PASSWORD, ""))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to close SAP action signer: cleanup failed"
    );
    assert_eq!(harness.saved_account().await, Some(expected_account()));
    assert_eq!(fake.signer.close_calls.get(), 1);
}

#[tokio::test]
async fn follows_relative_and_pod_redirects_within_the_redirect_budget() {
    let harness = Harness::new();
    let pod = format!("https://p7-buy.itunes.apple.com{AUTH_PATH}");
    let urls = [
        AUTH.to_owned(),
        format!("{AUTH}/"),
        pod.clone(),
        format!("{pod}/"),
        format!("{pod}/?routing=opaque"),
    ];
    let locations = [
        format!("{AUTH_PATH}/"),
        format!("//p7-buy.itunes.apple.com{AUTH_PATH}"),
        "authenticate/".to_owned(),
        "?routing=opaque".to_owned(),
    ];
    let original = Rc::new(RefCell::new(None));
    let fake = Fake::new(move |call, request| {
        assert_eq!(request.url, urls[call]);
        let mut original = original.borrow_mut();
        let original = original.get_or_insert_with(|| request.payload.clone());
        assert_eq!(request.payload, *original);
        if let Some(location) = locations.get(call) {
            let status = [301, 302, 307, 308][call];
            return result(status, &[("Location", location)], LoginResult::default());
        }
        result(200, &[(HTTP_HEADER_STORE_FRONT, "storefront")], success())
    });
    let mut signer = FakeSigner(Rc::clone(&fake.signer));
    harness
        .login(&fake)
        .login(&credentials("email", ""), AUTH, &mut signer)
        .await
        .unwrap();
    assert_eq!(fake.calls.get(), 5);
}

#[tokio::test]
async fn stops_a_redirect_loop_without_retaining_response_secrets() {
    let harness = Harness::new();
    let location = format!("{AUTH}?token=secret-query");
    let fake = Fake::always(move || {
        result(
            301,
            &[("Location", &location), ("Set-Cookie", "secret-cookie")],
            LoginResult {
                password_token: "secret-token".to_owned(),
                ..LoginResult::default()
            },
        )
    });
    let mut signer = FakeSigner(Rc::clone(&fake.signer));
    let error = harness
        .login(&fake)
        .login(&credentials("email", ""), AUTH, &mut signer)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "too many authentication redirects (HTTP 301)"
    );
    let Error::AuthRedirect {
        status,
        destination,
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!((*status, destination.as_str()), (301, AUTH));
    assert_eq!(fake.calls.get(), 5, "the redirect budget is four");
}

#[tokio::test]
async fn retries_the_first_credential_failure_at_the_assigned_pod() {
    let harness = Harness::new();
    let pod = format!("https://p7-buy.itunes.apple.com{AUTH_PATH}/");
    let fake = Fake::new(move |call, request| match call {
        0 => result(302, &[("Location", &pod)], LoginResult::default()),
        1 => {
            assert_eq!(request.url, pod);
            assert_eq!(payload(request, "attempt"), "1");
            result(200, &[], failure(FAILURE_TYPE_INVALID_CREDENTIALS, ""))
        }
        _ => {
            assert_eq!(request.url, pod);
            assert_eq!(payload(request, "attempt"), "2");
            result(
                200,
                &[],
                failure(FAILURE_TYPE_INVALID_CREDENTIALS, "invalid credentials"),
            )
        }
    });
    let mut signer = FakeSigner(Rc::clone(&fake.signer));
    let error = harness
        .login(&fake)
        .login(&credentials("email", ""), AUTH, &mut signer)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "invalid credentials");
    assert_eq!(fake.calls.get(), 3);
}

#[tokio::test]
async fn rejects_unsafe_or_unsupported_redirects_before_reposting_credentials() {
    for (status, location) in [
        (301, format!("http://buy.itunes.apple.com{AUTH_PATH}")),
        (
            302,
            format!("https://example.com{AUTH_PATH}?token=secret-query"),
        ),
        (
            307,
            format!("https://buy.itunes.apple.com.example.com{AUTH_PATH}"),
        ),
        (308, format!("//example.com{AUTH_PATH}")),
        (301, "https://buy.itunes.apple.com/other".to_owned()),
        (
            302,
            format!("https://user:secret-password@buy.itunes.apple.com{AUTH_PATH}"),
        ),
        (307, format!("https://buy.itunes.apple.com:8443{AUTH_PATH}")),
        (
            308,
            "https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/%61uthenticate".to_owned(),
        ),
        (301, format!("{AUTH}#secret-fragment")),
        (302, format!("{AUTH}?routing=opaque#secret-fragment")),
        (302, "https://buy.itunes.apple.com/%secret".to_owned()),
        (307, String::new()),
        (308, " \t".to_owned()),
        (303, format!("{AUTH}?token=secret-query")),
        (300, AUTH.to_owned()),
    ] {
        let harness = Harness::new();
        let response_location = location.clone();
        let fake = Fake::always(move || {
            result(
                status,
                &[
                    ("Location", &response_location),
                    ("Set-Cookie", "secret-cookie"),
                ],
                LoginResult {
                    password_token: "secret-token".to_owned(),
                    ..LoginResult::default()
                },
            )
        });
        let mut signer = FakeSigner(Rc::clone(&fake.signer));
        let error = harness
            .login(&fake)
            .login(&credentials("email", ""), AUTH, &mut signer)
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("secret"), "{location}: {error}");
        let Error::AuthRedirect { destination, .. } = &error else {
            panic!("{location}: {error:?}");
        };
        assert!(!destination.contains("secret"), "{location}: {destination}");
        assert_eq!(fake.calls.get(), 1, "{location}");
    }
}

#[test]
fn redacts_redirect_destinations_like_go() {
    for (location, destination) in [
        (format!("{AUTH}?token=secret#frag"), AUTH.to_owned()),
        (
            "https://user:pw@example.com:8443/a?b".to_owned(),
            "https://example.com:8443/a".to_owned(),
        ),
        ("//example.com/a".to_owned(), "//example.com/a".to_owned()),
        ("authenticate/".to_owned(), "authenticate/".to_owned()),
        (String::new(), String::new()),
        (
            "https://buy.itunes.apple.com/%secret".to_owned(),
            "invalid URL".to_owned(),
        ),
        ("mailto:someone".to_owned(), "mailto:".to_owned()),
    ] {
        assert_eq!(redacted_destination(&location), destination, "{location}");
    }
}

#[derive(Debug, thiserror::Error)]
#[error("failed to make round trip")]
struct RoundTrip(#[source] std::io::Error);

#[tokio::test]
async fn bounds_transport_retries_and_preserves_the_underlying_error() {
    use std::io::ErrorKind;
    let cases: Vec<(&str, Box<dyn Fn() -> Error>, usize)> = vec![
        (
            "request deadline",
            Box::new(|| transport(std::io::Error::from(ErrorKind::TimedOut), "")),
            3,
        ),
        (
            "deadline wrapped by the transport",
            Box::new(|| transport(RoundTrip(ErrorKind::TimedOut.into()), "")),
            3,
        ),
        (
            "connection closed",
            Box::new(|| transport(std::io::Error::from(ErrorKind::UnexpectedEof), "")),
            3,
        ),
        (
            "connection reset",
            Box::new(|| transport(std::io::Error::from(ErrorKind::ConnectionReset), "")),
            3,
        ),
        (
            "broken pipe",
            Box::new(|| transport(std::io::Error::from(ErrorKind::BrokenPipe), "")),
            3,
        ),
        (
            "permanent DNS failure",
            Box::new(|| transport(std::io::Error::from(ErrorKind::NotFound), "")),
            1,
        ),
        (
            "connection refused",
            Box::new(|| transport(std::io::Error::from(ErrorKind::ConnectionRefused), "")),
            1,
        ),
        (
            "signing failure",
            Box::new(|| Error::Invalid("deadline exceeded".to_owned())),
            1,
        ),
        (
            "other transport failure",
            Box::new(|| transport(std::io::Error::other("unrecoverable transport error"), "")),
            1,
        ),
    ];
    for (name, failure, calls) in cases {
        let harness = Harness::new();
        let cause = failure().to_string();
        let fake = Fake::always(move || Err(failure()));
        let error = harness
            .login(&fake)
            .send_authentication_request(
                &AuthRequest::default(),
                &mut FakeSigner(Rc::clone(&fake.signer)),
            )
            .await
            .unwrap_err();
        assert_eq!(fake.calls.get(), calls, "{name}");
        assert!(error.to_string().ends_with(&cause), "{name}: {error}");
        if calls == 1 {
            assert!(fake.waits().is_empty(), "{name}");
        } else {
            assert_eq!(
                fake.waits(),
                [Duration::from_secs(10), Duration::from_secs(20)],
                "{name}"
            );
            assert!(
                error.to_string().contains(
                    "after 3 attempts (transport error, transport error, transport error)"
                ),
                "{name}: {error}"
            );
        }
    }
}

#[tokio::test]
async fn shares_the_attempt_budget_with_http_failures_and_honors_retry_after() {
    for status in [403, 503] {
        let harness = Harness::new();
        let fake = Fake::new(move |call, _| {
            Err(match call {
                0 => transport(std::io::Error::from(std::io::ErrorKind::UnexpectedEof), ""),
                1 => unexpected(status, "2"),
                _ => transport(std::io::Error::from(std::io::ErrorKind::TimedOut), ""),
            })
        });
        let error = harness
            .login(&fake)
            .send_authentication_request(
                &AuthRequest::default(),
                &mut FakeSigner(Rc::clone(&fake.signer)),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::Context { source, .. } if matches!(**source, Error::Transport(_)))
        );
        assert_eq!(
            fake.waits(),
            [Duration::from_secs(10), Duration::from_secs(2)]
        );
        assert!(
            error.to_string().contains(&format!(
                "after 3 attempts (transport error, HTTP {status}, transport error)"
            )),
            "{error}"
        );
    }
}

#[tokio::test]
async fn stops_when_an_incomplete_response_requests_a_wait_beyond_the_budget() {
    let harness = Harness::new();
    let mut fake = Fake::always(|| {
        Err(transport(
            std::io::Error::from(std::io::ErrorKind::TimedOut),
            "3600",
        ))
    });
    fake.sleep_forbidden = true;
    let error = harness
        .login(&fake)
        .send_authentication_request(
            &AuthRequest::default(),
            &mut FakeSigner(Rc::clone(&fake.signer)),
        )
        .await
        .unwrap_err();
    assert_eq!(fake.calls.get(), 1);
    assert_eq!(
        error.to_string(),
        "apple requested a wait longer than 30s; try again later: request failed: timed out"
    );
}

#[tokio::test]
async fn honors_retry_after_without_exceeding_the_wait_budget() {
    let secs = Duration::from_secs;
    for (status, header, calls, waits) in [
        (503, "5", 3, vec![secs(5), secs(5)]),
        (429, "3", 3, vec![secs(3), secs(3)]),
        (429, "", 3, vec![secs(10), secs(20)]),
        (429, "invalid", 3, vec![secs(10), secs(20)]),
        (503, "", 3, vec![secs(10), secs(20)]),
        (204, "", 3, vec![secs(10), secs(20)]),
        (404, "", 3, vec![secs(10), secs(20)]),
        (429, "0", 3, vec![secs(1), secs(1)]),
        (503, "3600", 1, vec![]),
        (403, "", 3, vec![secs(10), secs(20)]),
        (403, "1", 3, vec![secs(1), secs(1)]),
        (403, "3600", 1, vec![]),
        (302, "", 1, vec![]),
    ] {
        let harness = Harness::new();
        let fake = Fake::always(move || Err(unexpected(status, header)));
        let error = harness
            .login(&fake)
            .send_authentication_request(
                &AuthRequest::default(),
                &mut FakeSigner(Rc::clone(&fake.signer)),
            )
            .await
            .unwrap_err();
        assert_eq!(unexpected_status(&error), Some(status));
        assert_eq!(fake.calls.get(), calls, "{status} {header:?}");
        assert_eq!(fake.waits(), waits, "{status} {header:?}");
    }
}

#[test]
fn parses_dates_and_seconds_without_overflow_or_negative_waits() {
    use time::macros::datetime;
    let now = datetime!(2026-09-13 12:00:00 UTC);
    for (value, want) in [
        ("Sun, 13 Sep 2026 12:00:05 GMT", Duration::from_secs(5)),
        ("Sun, 13 Sep 2026 11:59:59 GMT", Duration::ZERO),
        ("Sunday, 13-Sep-26 12:00:07 GMT", Duration::from_secs(7)),
        ("Sun Sep 13 12:00:09 2026", Duration::from_secs(9)),
        ("0", Duration::ZERO),
        (" 5 ", Duration::from_secs(5)),
        (
            "18446744073709551615",
            MAX_AUTHENTICATION_RETRY_DELAY + Duration::from_secs(1),
        ),
    ] {
        assert_eq!(
            authentication_retry_after(value, now),
            Some(want),
            "{value}"
        );
    }
    for value in ["", "-1", "+5", "invalid", "18446744073709551616"] {
        assert_eq!(authentication_retry_after(value, now), None, "{value}");
    }
}

#[tokio::test]
async fn does_not_retry_populated_apple_credential_errors_or_2fa_challenges() {
    for status in [200, 403] {
        for data in [
            failure(FAILURE_TYPE_INVALID_CREDENTIALS, "invalid credentials"),
            failure("", CUSTOMER_MESSAGE_BAD_LOGIN),
        ] {
            let harness = Harness::new();
            let mut fake = Fake::always(move || result(status, &[], data.clone()));
            fake.sleep_forbidden = true;
            let response = harness
                .login(&fake)
                .send_authentication_request(
                    &AuthRequest::default(),
                    &mut FakeSigner(Rc::clone(&fake.signer)),
                )
                .await
                .unwrap();
            let Err(error) = parse_login_response(&response, 2, "", AUTH) else {
                panic!("{status}: accepted");
            };
            assert!(!error.to_string().contains("another network"), "{error}");
            assert_eq!(fake.calls.get(), 1);
        }
    }
}

#[test]
fn reports_machine_actions_before_other_responses() {
    let response = result(
        200,
        &[
            ("x-apple-amd-action", " RP "),
            ("X-Apple-AMD-Data", " data "),
        ],
        failure("", CUSTOMER_MESSAGE_BAD_LOGIN),
    )
    .unwrap();
    let Err(error) = parse_login_response(&response, 1, "", AUTH) else {
        panic!("accepted");
    };
    assert_eq!(
        error.machine_action(),
        Some(&MachineAction {
            action: "RP".to_owned(),
            data: "data".to_owned(),
        })
    );
    assert_eq!(
        error.to_string(),
        "apple requested an authentication machine action: RP"
    );
}

#[test]
fn reads_the_dsid_from_headers_and_cookies() {
    let headers = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    assert_eq!(
        authentication_response_dsid(&headers(&[("x-dsid", " 42 ")])),
        "42"
    );
    assert_eq!(
        authentication_response_dsid(&headers(&[(
            "set-cookie",
            "a=1; mz_at_ssl-123=x; mz_at0-9=y"
        )])),
        "123"
    );
    assert_eq!(
        authentication_response_dsid(&headers(&[("set-cookie", "xmz_at0-1=y")])),
        ""
    );
}

#[tokio::test]
async fn falls_back_to_the_saved_or_request_storefront() {
    let harness = Harness::new();
    let fake = Fake::always(|| unreachable!());
    let response = |dsid: &str| {
        let mut data = success();
        dsid.clone_into(&mut data.directory_services_id);
        result(
            200,
            &[(HTTP_HEADER_REQUEST_STORE_FRONT, "143441-1,29")],
            data,
        )
        .unwrap()
    };
    let login = harness.login(&fake);
    assert_eq!(
        login.login_storefront(&response("1"), "1").await.unwrap(),
        "143441-1,29"
    );
    let saved = Account {
        directory_services_id: "1".to_owned(),
        store_front: "143443-4,29".to_owned(),
        ..Account::default()
    };
    harness
        .keychain
        .set(keys::ACCOUNT, &serde_json::to_vec(&saved).unwrap())
        .await
        .unwrap();
    assert_eq!(
        login.login_storefront(&response("1"), "1").await.unwrap(),
        "143443-4,29"
    );
    assert_eq!(
        login.login_storefront(&response("2"), "2").await.unwrap(),
        "143441-1,29"
    );
    let missing = result(200, &[], success()).unwrap();
    assert_eq!(
        login
            .login_storefront(&missing, "2")
            .await
            .unwrap_err()
            .to_string(),
        "failed to get storefront header: header not found"
    );
}

#[test]
fn sends_anisette_headers_with_store_names() {
    let harness = Harness::new();
    let fake = Fake::always(|| unreachable!());
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("X-Apple-I-MD", "otp"),
        ("X-Apple-I-MD-M", "machine"),
        ("X-Apple-I-MD-RINFO", "17106176"),
        ("X-Mme-Client-Info", "<info>"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    let login = Login {
        authentication_headers: Some(&headers),
        ..harness.login(&fake)
    };
    let request = login.login_request(&credentials("person@example.test", "123 456"), AUTH, 2);
    let names: Vec<(&str, &str)> = request
        .headers
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("Content-Type", "application/x-apple-plist"),
            ("User-Agent", STORE_USER_AGENT),
            ("X-Apple-AMD", "otp"),
            ("X-Apple-AMD-M", "machine"),
            ("X-Apple-I-Md-Rinfo", "17106176"),
            ("X-Mme-Client-Info", "<info>"),
        ]
    );
    let payload: Vec<(&str, &str)> = request
        .payload
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_string().unwrap()))
        .collect();
    assert_eq!(
        payload,
        [
            ("appleId", "person@example.test"),
            ("attempt", "2"),
            ("guid", "guid"),
            ("password", "password123456"),
            ("rmp", "0"),
            ("why", "signIn"),
        ]
    );
}
