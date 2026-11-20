//! Legacy App Store login (`pkg/appstore/appstore_login.go`), with the fork's handling of
//! invalid 2FA codes (`fix/invalid-auth-code`).

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;

use ipatool_keychain::Keychain;
use ipatool_sap::signer::{self, ActionSigner, Signer};
use plist::{Dictionary, Value};
use regex::Regex;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use time::OffsetDateTime;
use url::Url;

use crate::auth_http::{
    AuthRequest, AuthResult, is_authentication_redirect, retryable_transport_error,
};
use crate::bag::validate_auth_endpoint;
use crate::login_storefront::phone_number_storefront_country_code;
use crate::store::STORE_USER_AGENT;
use crate::{Account, Client, Error, MachineAction, Result, SapConfig, keys, machine};

const MAX_AUTHENTICATION_REQUEST_ATTEMPTS: u32 = 3;
const MAX_AUTHENTICATION_REDIRECTS: u32 = 4;
const AUTHENTICATION_RETRY_DELAY: Duration = Duration::from_secs(10);
const MAX_AUTHENTICATION_RETRY_DELAY: Duration = Duration::from_secs(30);

const FAILURE_TYPE_INVALID_CREDENTIALS: &str = "-5000";
const FAILURE_TYPE_INVALID_AUTH_CODE: &str = "5005";
const CUSTOMER_MESSAGE_BAD_LOGIN: &str = "MZFinance.BadLogin.Configurator_message";
const CUSTOMER_MESSAGE_ACCOUNT_DISABLED: &str = "Your account is disabled.";

const HTTP_HEADER_STORE_FRONT: &str = "X-Set-Apple-Store-Front";
const HTTP_HEADER_REQUEST_STORE_FRONT: &str = "X-Apple-Request-Store-Front";
const HTTP_HEADER_POD: &str = "pod";

static STORE_COOKIE_DSID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[ ,])mz_at(?:0|_ssl)-([0-9]+)=").expect("valid regex"));

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginInput {
    /// Apple ID: an email address or a phone number.
    pub email: String,
    pub password: String,
    /// 2FA code; empty on the first attempt.
    pub auth_code: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginOutput {
    pub account: Account,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LoginAddressResult {
    first_name: String,
    last_name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct LoginAccountResult {
    #[serde(rename = "appleId")]
    email: String,
    address: LoginAddressResult,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct LoginDialogActionResult {
    kind: String,
    url: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LoginDialogResult {
    message: String,
    explanation: String,
    ok_button_string: String,
    ok_button_action: LoginDialogActionResult,
    cancel_button_string: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct LoginResult {
    failure_type: String,
    customer_message: String,
    #[serde(rename = "accountInfo")]
    account: LoginAccountResult,
    dialog: LoginDialogResult,
    #[serde(rename = "dsPersonId")]
    directory_services_id: String,
    password_token: String,
}

/// What login needs from the outside world. Tests replace it as Go's tests replace the
/// machine, bag client, login client, signer factory and retry sleep.
pub(crate) trait LoginBackend {
    type Signer: ActionSigner;
    fn mac_address(&self) -> Result<String>;
    async fn sap_config(&self, client: &Client, guid: &str) -> Result<SapConfig>;
    /// Go's `ActionSignerFactory`.
    async fn create_signer(
        &self,
        client: &Client,
        config: &SapConfig,
        machine_id: &[u8],
    ) -> Result<Self::Signer>;
    async fn send(
        &self,
        client: &Client,
        request: &AuthRequest,
        signer: &mut Self::Signer,
    ) -> Result<AuthResult<LoginResult>>;
    async fn sleep(&self, delay: Duration);
}

struct Live;

impl LoginBackend for Live {
    type Signer = Signer;

    fn mac_address(&self) -> Result<String> {
        machine::mac_address()
    }

    async fn sap_config(&self, client: &Client, guid: &str) -> Result<SapConfig> {
        client.sap_config(guid).await
    }

    /// Go's `frameworkActionSignerFactory`. SAP setup shares the Store cookie jar.
    async fn create_signer(
        &self,
        client: &Client,
        config: &SapConfig,
        machine_id: &[u8],
    ) -> Result<Signer> {
        Signer::new(signer::Config {
            http_client: Some(client.store.clone()),
            setup_url: config.setup_url.clone(),
            certificate_url: config.certificate_url.clone(),
            version: config.version,
            hardware_id: machine_id.to_vec(),
        })
        .await
        .map_err(|err| Error::from(err).context("create SAP signer"))
    }

    async fn send(
        &self,
        client: &Client,
        request: &AuthRequest,
        signer: &mut Signer,
    ) -> Result<AuthResult<LoginResult>> {
        client.send_authentication(request, signer).await
    }

    async fn sleep(&self, delay: Duration) {
        tokio::time::sleep(delay).await;
    }
}

impl Client {
    /// Go's `legacyLogin`: signs in with a SAP-signed request and saves the account in the
    /// keychain under `account`.
    ///
    /// `authentication_headers` are the anisette headers of the native flow (Go's
    /// `authenticationHeaders`); `X-Apple-I-MD` and `X-Apple-I-MD-M` are sent as
    /// `X-Apple-AMD` and `X-Apple-AMD-M`. `None` is Go's legacy login.
    ///
    /// The signer runs Apple's code on this thread, so the future is not `Send`.
    ///
    /// # Errors
    /// [`Error::AuthCodeRequired`] and [`Error::InvalidAuthCode`] (see
    /// [`Error::is_auth_code_required`], [`Error::is_invalid_auth_code`]) ask for a 2FA code;
    /// [`Error::machine_action`] reports Apple's provisioning request. When only closing the
    /// signer fails, the account has already been saved.
    pub async fn login(
        &self,
        input: &LoginInput,
        keychain: &Keychain,
        authentication_headers: Option<&HeaderMap>,
    ) -> Result<LoginOutput> {
        Login {
            client: self,
            backend: &Live,
            keychain,
            authentication_headers,
        }
        .legacy_login(input)
        .await
    }
}

struct Login<'a, B> {
    client: &'a Client,
    backend: &'a B,
    keychain: &'a Keychain,
    authentication_headers: Option<&'a HeaderMap>,
}

struct Credentials<'a> {
    email: &'a str,
    password: &'a str,
    auth_code: &'a str,
    guid: &'a str,
}

enum Next {
    Done,
    Retry,
    Redirect(String),
}

impl<B: LoginBackend> Login<'_, B> {
    async fn legacy_login(&self, input: &LoginInput) -> Result<LoginOutput> {
        let auth_code = normalize_auth_code(&input.auth_code)?;
        let country = phone_number_storefront_country_code(&input.email);
        if !country.is_empty() {
            self.client
                .discover_storefront_id(country)
                .await
                .map_err(|err| err.context("failed to discover storefronts"))?;
        }
        let mac_address = self
            .backend
            .mac_address()
            .map_err(|err| err.context("failed to get mac address"))?;
        let (guid, machine_id) = machine::identity(&mac_address)?;
        let config = self
            .backend
            .sap_config(self.client, &guid)
            .await
            .map_err(|err| err.context("failed to get bag"))?;
        let mut signer = self
            .backend
            .create_signer(self.client, &config, &machine_id)
            .await
            .map_err(|err| err.context("failed to initialize SAP action signer"))?;
        let credentials = Credentials {
            email: &input.email,
            password: &input.password,
            auth_code: &auth_code,
            guid: &guid,
        };
        let login = self
            .login(&credentials, &config.auth_endpoint, &mut signer)
            .await;
        let close = signer
            .close()
            .map_err(|err| Error::from(err).context("failed to close SAP action signer"));
        match (login, close) {
            (Ok(account), Ok(())) => Ok(LoginOutput { account }),
            (Ok(_), Err(close)) => Err(close),
            (Err(login), Ok(())) => Err(login),
            (Err(login), Err(close)) => Err(Error::Joined(vec![login, close])),
        }
    }

    async fn login(
        &self,
        credentials: &Credentials<'_>,
        endpoint: &str,
        signer: &mut B::Signer,
    ) -> Result<Account> {
        let mut request = self.login_request(credentials, endpoint, 1);
        let mut redirects = 0;
        let mut attempt = 1;
        let result = loop {
            let result = match self.send_authentication_request(&request, signer).await {
                Ok(result) => result,
                Err(err) => {
                    let mut stage = if credentials.auth_code.is_empty() {
                        "sign-in".to_owned()
                    } else {
                        "2FA verification".to_owned()
                    };
                    if redirects > 0 {
                        stage.push_str(" at Store pod");
                    }
                    return Err(err.context(format!("{stage} request failed")));
                }
            };
            match parse_login_response(&result, attempt, credentials.auth_code, &request.url)? {
                Next::Done => break result,
                Next::Redirect(location) => {
                    if redirects == MAX_AUTHENTICATION_REDIRECTS {
                        return Err(authentication_redirect_error(
                            Error::Invalid("too many authentication redirects".to_owned()),
                            result.status,
                            &location,
                        ));
                    }
                    redirects += 1;
                    // Redirects belong to the same attempt and keep its payload.
                    request.url = location;
                }
                Next::Retry => {
                    attempt += 1;
                    request = self.login_request(credentials, &request.url, attempt);
                }
            }
        };
        self.save_account(credentials, &result).await
    }

    /// The end of Go's `login`: builds the account from Apple's answer and saves it.
    async fn save_account(
        &self,
        credentials: &Credentials<'_>,
        result: &AuthResult<LoginResult>,
    ) -> Result<Account> {
        let store_error = |message: String| Error::Store {
            message,
            status: result.status,
        };
        let pod = result
            .header(HTTP_HEADER_POD)
            .unwrap_or_default()
            .to_owned();
        let mut directory_services_id = result.data.directory_services_id.clone();
        if directory_services_id.is_empty() {
            directory_services_id = authentication_response_dsid(&result.headers);
        }
        if directory_services_id.is_empty() {
            return Err(store_error(
                "apple returned no directory services identifier".to_owned(),
            ));
        }
        let store_front = self
            .login_storefront(result, &directory_services_id)
            .await
            .map_err(|err| store_error(err.to_string()))?;
        let address = &result.data.account.address;
        let email = if result.data.account.email.is_empty() {
            credentials.email.to_owned()
        } else {
            result.data.account.email.clone()
        };
        let account = Account {
            name: format!("{} {}", address.first_name, address.last_name),
            email,
            password_token: result.data.password_token.clone(),
            directory_services_id,
            store_front,
            password: credentials.password.to_owned(),
            pod,
        };
        let data = serde_json::to_vec(&account)
            .map_err(|err| Error::Invalid(format!("failed to marshal json: {err}")))?;
        self.keychain
            .set(keys::ACCOUNT, &data)
            .await
            .map_err(|err| Error::from(err).context("failed to save account in keychain"))?;
        Ok(account)
    }

    /// Go's `loginStorefront` (`account_storefront.go`).
    async fn login_storefront(
        &self,
        result: &AuthResult<LoginResult>,
        dsid: &str,
    ) -> Result<String> {
        if let Some(store_front) = result.header(HTTP_HEADER_STORE_FRONT)
            && !store_front.trim().is_empty()
        {
            return Ok(store_front.to_owned());
        }
        // The request-storefront header is an echo; keep a saved storefront for the same DSID.
        match self.keychain.get(keys::ACCOUNT).await {
            Ok(data) => {
                let saved: Account = serde_json::from_slice(&data)
                    .map_err(|err| Error::Invalid(format!("failed to unmarshal json: {err}")))?;
                if !dsid.is_empty()
                    && saved.directory_services_id == dsid
                    && !saved.store_front.is_empty()
                {
                    return Ok(saved.store_front);
                }
            }
            Err(ipatool_keychain::Error::NotFound) => {}
            Err(err) => return Err(Error::from(err).context("failed to get account")),
        }
        result
            .header(HTTP_HEADER_REQUEST_STORE_FRONT)
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::Invalid("failed to get storefront header: header not found".to_owned())
            })
    }

    async fn send_authentication_request(
        &self,
        request: &AuthRequest,
        signer: &mut B::Signer,
    ) -> Result<AuthResult<LoginResult>> {
        let mut statuses = Vec::new();
        let mut only_http = true;
        let mut attempt = 1;
        loop {
            let error = match self.backend.send(self.client, request, signer).await {
                Ok(result) => return Ok(result),
                Err(error) => error,
            };
            let Some((status, retry_after)) = retryable_authentication_error(&error) else {
                return Err(authentication_request_error(error));
            };
            if status == 0 {
                statuses.push("transport error".to_owned());
                only_http = false;
            } else {
                statuses.push(format!("HTTP {status}"));
            }
            if attempt == MAX_AUTHENTICATION_REQUEST_ATTEMPTS {
                let mut summary = statuses.join(", ");
                if only_http {
                    summary = summary.replace(", HTTP ", ", ");
                }
                return Err(authentication_request_error(error).context(format!(
                    "authentication request failed after {MAX_AUTHENTICATION_REQUEST_ATTEMPTS} attempts ({summary})"
                )));
            }
            let mut delay = (AUTHENTICATION_RETRY_DELAY * (1 << (attempt - 1)))
                .min(MAX_AUTHENTICATION_RETRY_DELAY);
            if let Some(requested) =
                authentication_retry_after(&retry_after, OffsetDateTime::now_utc())
            {
                if requested > MAX_AUTHENTICATION_RETRY_DELAY {
                    return Err(error.context(format!(
                        "apple requested a wait longer than {}s; try again later",
                        MAX_AUTHENTICATION_RETRY_DELAY.as_secs()
                    )));
                }
                // Retry-After takes precedence over the fallback backoff.
                delay = requested.max(Duration::from_secs(1));
            }
            self.backend.sleep(delay).await;
            attempt += 1;
        }
    }

    fn login_request(
        &self,
        credentials: &Credentials<'_>,
        endpoint: &str,
        attempt: u32,
    ) -> AuthRequest {
        let mut headers = BTreeMap::from([
            (
                "Content-Type".to_owned(),
                "application/x-apple-plist".to_owned(),
            ),
            ("User-Agent".to_owned(), STORE_USER_AGENT.to_owned()),
        ]);
        if let Some(authentication) = self.authentication_headers {
            for name in authentication.keys() {
                if name == "x-apple-i-md" || name == "x-apple-i-md-m" {
                    continue;
                }
                let values: Vec<&str> = authentication
                    .get_all(name)
                    .iter()
                    .map(|value| value.to_str().unwrap_or_default())
                    .collect();
                headers.insert(canonical_header_key(name.as_str()), values.join(", "));
            }
            for (from, to) in [
                ("X-Apple-I-MD", "X-Apple-AMD"),
                ("X-Apple-I-MD-M", "X-Apple-AMD-M"),
            ] {
                if let Some(value) = authentication
                    .get(from)
                    .and_then(|value| value.to_str().ok())
                    && !value.is_empty()
                {
                    headers.insert(to.to_owned(), value.to_owned());
                }
            }
        }
        let country = phone_number_storefront_country_code(credentials.email);
        if !country.is_empty() {
            let store_front = self.client.storefront_id(country);
            if !store_front.is_empty() {
                headers.insert("X-Apple-Store-Front".to_owned(), store_front);
            }
        }
        let mut payload = Dictionary::new();
        for (key, value) in [
            ("appleId", credentials.email.to_owned()),
            ("attempt", attempt.to_string()),
            ("guid", credentials.guid.to_owned()),
            (
                "password",
                format!(
                    "{}{}",
                    credentials.password,
                    credentials.auth_code.replace(' ', "")
                ),
            ),
            ("rmp", "0".to_owned()),
            ("why", "signIn".to_owned()),
        ] {
            payload.insert(key.to_owned(), Value::String(value));
        }
        AuthRequest {
            url: endpoint.to_owned(),
            headers,
            payload,
        }
    }
}

/// Go's `normalizeAuthCode`: strips whitespace and one matched pair of bracketed-paste
/// markers, then requires exactly six ASCII digits.
fn normalize_auth_code(code: &str) -> Result<String> {
    if code.is_empty() {
        return Ok(String::new());
    }
    let mut code = code.trim();
    if let Some(inner) = code
        .strip_prefix("\x1b[200~")
        .and_then(|rest| rest.strip_suffix("\x1b[201~"))
    {
        code = inner;
    }
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Invalid(
            "2FA code must contain exactly six digits".to_owned(),
        ));
    }
    Ok(code)
}

/// Go's `retryableAuthenticationError`: the HTTP status (0 for transport failures) and
/// Apple's `Retry-After` when the error is worth another attempt.
fn retryable_authentication_error(error: &Error) -> Option<(u16, String)> {
    match error {
        Error::Context { source, .. } => retryable_authentication_error(source),
        // A Store pod can return a transient HTML 403. Decoded Apple errors never get here.
        Error::UnexpectedResponse(response) => {
            matches!(response.status, 204 | 403 | 404 | 429 | 500..=599)
                .then(|| (response.status, response.retry_after.clone()))
        }
        Error::Transport(transport) => {
            retryable_transport_error(transport).then(|| (0, transport.retry_after.clone()))
        }
        _ => None,
    }
}

fn unexpected_status(error: &Error) -> Option<u16> {
    match error {
        Error::Context { source, .. } => unexpected_status(source),
        Error::UnexpectedResponse(response) => Some(response.status),
        _ => None,
    }
}

/// Go's `authenticationRequestError`.
fn authentication_request_error(error: Error) -> Error {
    match unexpected_status(&error) {
        None => error,
        Some(429) => error.context("apple rate limited authentication; try again later"),
        Some(_) => error.context(
            "apple returned no usable authentication response; try again later or from another network",
        ),
    }
}

/// Go's `authenticationRetryAfter`: whole seconds or an HTTP date. A wait over the budget
/// saturates to just above it so the caller stops instead of retrying early.
fn authentication_retry_after(value: &str, now: OffsetDateTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && let Ok(seconds) = value.parse::<u64>()
    {
        if seconds > MAX_AUTHENTICATION_RETRY_DELAY.as_secs() {
            return Some(MAX_AUTHENTICATION_RETRY_DELAY + Duration::from_secs(1));
        }
        return Some(Duration::from_secs(seconds));
    }
    let date = parse_http_time(value)?;
    Some((date - now).try_into().unwrap_or(Duration::ZERO))
}

/// Go's `http.ParseTime`: RFC 1123 with GMT, RFC 850 and ANSI C dates.
fn parse_http_time(value: &str) -> Option<OffsetDateTime> {
    use time::PrimitiveDateTime;
    use time::macros::format_description;

    let rfc1123 = format_description!(
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
    );
    let ansic = format_description!(
        "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
    );
    if let Ok(date) =
        PrimitiveDateTime::parse(value, rfc1123).or_else(|_| PrimitiveDateTime::parse(value, ansic))
    {
        return Some(date.assume_utc());
    }
    // RFC 850 has a two-digit year, which Go maps to 1969-2068.
    let (_, rest) = value.split_once(", ")?;
    let (date, time) = rest.split_once(' ')?;
    let mut parts = date.splitn(3, '-');
    let (day, month, year) = (parts.next()?, parts.next()?, parts.next()?);
    if year.len() != 2 {
        return None;
    }
    let year: u16 = year.parse().ok()?;
    let year = if year >= 69 { 1900 + year } else { 2000 + year };
    let expanded = format!("{day}-{month}-{year} {time}");
    PrimitiveDateTime::parse(
        &expanded,
        format_description!("[day]-[month repr:short]-[year] [hour]:[minute]:[second] GMT"),
    )
    .ok()
    .map(PrimitiveDateTime::assume_utc)
}

fn parse_login_response(
    result: &AuthResult<LoginResult>,
    attempt: u32,
    auth_code: &str,
    endpoint: &str,
) -> Result<Next> {
    let status = result.status;
    let data = &result.data;
    let store_error = |message: String| Err(Error::Store { message, status });
    if let Some(action) = authentication_machine_action(&result.headers) {
        return Err(action.into());
    }
    if let Some(message) = authentication_dialog_error(data) {
        return store_error(message);
    }
    if (300..400).contains(&status) {
        let location = result.header("location");
        let redirect = match location {
            None => Err(Error::Invalid(
                "failed to retrieve redirect location: header not found".to_owned(),
            )),
            Some(_) if !is_authentication_redirect(status) => Err(Error::Invalid(
                "unsupported authentication redirect status".to_owned(),
            )),
            Some(location) => resolve_authentication_redirect(endpoint, location),
        };
        return redirect.map(Next::Redirect).map_err(|err| {
            authentication_redirect_error(err, status, location.unwrap_or_default())
        });
    }
    let failure = data.failure_type.as_str();
    let message = data.customer_message.as_str();
    if attempt == 1 && failure == FAILURE_TYPE_INVALID_CREDENTIALS {
        return Ok(Next::Retry);
    }
    if failure == FAILURE_TYPE_INVALID_AUTH_CODE
        || (failure == FAILURE_TYPE_INVALID_CREDENTIALS && !auth_code.is_empty())
    {
        return Err(Error::InvalidAuthCode(None));
    }
    if failure.is_empty() && message == CUSTOMER_MESSAGE_BAD_LOGIN {
        return Err(if auth_code.is_empty() {
            Error::AuthCodeRequired
        } else {
            Error::InvalidAuthCode(Some(
                "apple did not complete verification; try a fresh 2FA code",
            ))
        });
    }
    if failure.is_empty() && message == CUSTOMER_MESSAGE_ACCOUNT_DISABLED {
        return store_error("account is disabled".to_owned());
    }
    if !failure.is_empty() {
        if message.is_empty() {
            return store_error(format!("something went wrong (failureType: {failure})"));
        }
        return store_error(message.to_owned());
    }
    if status != 200 || data.password_token.is_empty() {
        return Err(Error::Invalid(format!(
            "apple returned no usable authentication response (HTTP {status}): missing account credentials or unexpected status; try again later or from another network"
        )));
    }
    Ok(Next::Done)
}

/// Go's `authenticationDialogError`: Apple's dialog text, with the button URL only when it
/// is an `https` Apple URL.
fn authentication_dialog_error(result: &LoginResult) -> Option<String> {
    let dialog = &result.dialog;
    if [
        &dialog.message,
        &dialog.explanation,
        &dialog.ok_button_string,
        &dialog.cancel_button_string,
        &dialog.ok_button_action.kind,
        &dialog.ok_button_action.url,
    ]
    .iter()
    .all(|value| value.trim().is_empty())
    {
        return None;
    }
    let mut message = dialog.message.trim();
    let explanation = dialog.explanation.trim();
    if message.is_empty() {
        message = result.customer_message.trim();
    }
    let action_url = trusted_authentication_dialog_url(&dialog.ok_button_action);
    if message.is_empty() && explanation.is_empty() && action_url.is_none() {
        return None;
    }
    let mut parts = Vec::new();
    if !message.is_empty() {
        parts.push(message);
    }
    if !explanation.is_empty() && explanation != message {
        parts.push(explanation);
    }
    let mut detail = parts.join(": ");
    if detail.is_empty() {
        "Apple requires action before sign-in can continue".clone_into(&mut detail);
    }
    if let Some(url) = action_url {
        let mut label = dialog.ok_button_string.trim();
        if label.is_empty() {
            label = "Continue";
        }
        detail = format!("{detail} ({label}: {url})");
    }
    Some(detail)
}

fn trusted_authentication_dialog_url(action: &LoginDialogActionResult) -> Option<String> {
    if !action.kind.trim().eq_ignore_ascii_case("OpenURL") {
        return None;
    }
    let text = action.url.trim();
    let url = Url::parse(text).ok()?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let trusted = url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none_or(str::is_empty)
        && url.port().is_none()
        && (host == "apple.com" || host.ends_with(".apple.com"));
    trusted.then(|| text.to_owned())
}

fn authentication_machine_action(headers: &[(String, String)]) -> Option<MachineAction> {
    let mut action = String::new();
    let mut data = String::new();
    for (key, value) in headers {
        if key.eq_ignore_ascii_case("X-Apple-AMD-Action") {
            value.trim().clone_into(&mut action);
        } else if key.eq_ignore_ascii_case("X-Apple-AMD-Data") {
            value.trim().clone_into(&mut data);
        }
    }
    (!action.is_empty()).then_some(MachineAction { action, data })
}

/// Go's `authenticationResponseDSID`: `X-Dsid`, else the `mz_at0-`/`mz_at_ssl-` cookie name.
fn authentication_response_dsid(headers: &[(String, String)]) -> String {
    if let Some((_, value)) = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("X-Dsid"))
    {
        return value.trim().to_owned();
    }
    headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("Set-Cookie"))
        .find_map(|(_, value)| STORE_COOKIE_DSID.captures(value))
        .map_or_else(String::new, |captures| captures[1].to_owned())
}

/// Whether Go's `url.Parse` accepts `value`: no control characters and only valid escapes.
fn go_url_parses(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.iter().any(|b| *b < 0x20 || *b == 0x7f)
        && bytes.iter().enumerate().all(|(index, b)| {
            *b != b'%'
                || bytes
                    .get(index + 1..index + 3)
                    .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit))
        })
}

fn resolve_authentication_redirect(endpoint: &str, location: &str) -> Result<String> {
    let base = Url::parse(endpoint)
        .map_err(|_| Error::Invalid("invalid authentication redirect base URL".to_owned()))?;
    let location = location.trim();
    let invalid_location = || Error::Invalid("invalid authentication redirect location".to_owned());
    if location.is_empty() || !go_url_parses(location) {
        return Err(invalid_location());
    }
    let destination = base.join(location).map_err(|_| invalid_location())?;
    validate_auth_endpoint(destination.as_str())
        .map_err(|err| Error::Invalid(format!("invalid authentication redirect: {err}")))?;
    Ok(destination.into())
}

/// Go's `authenticationRedirectError`: routing diagnostics without URL credentials,
/// query, fragment, body or headers.
fn authentication_redirect_error(error: Error, status: u16, location: &str) -> Error {
    Error::AuthRedirect {
        source: Box::new(error),
        status,
        destination: redacted_destination(location),
    }
}

/// `url.URL{Scheme, Host, Path}.String()` of Go's `url.Parse(location)`.
fn redacted_destination(location: &str) -> String {
    if !go_url_parses(location) {
        return "invalid URL".to_owned();
    }
    let rest = location.split('#').next().unwrap_or_default();
    let rest = rest.split('?').next().unwrap_or_default();
    let (scheme, rest) = match rest.split_once(':') {
        Some((scheme, rest))
            if scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            (scheme.to_ascii_lowercase(), rest)
        }
        Some(("", _)) => return "invalid URL".to_owned(),
        _ => (String::new(), rest),
    };
    let (host, path) = if let Some(authority) = rest.strip_prefix("//") {
        let (authority, path) = authority
            .find('/')
            .map_or((authority, ""), |index| authority.split_at(index));
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (host, path)
    } else if !scheme.is_empty() && !rest.starts_with('/') {
        // An opaque URL such as `mailto:x` has no host or path.
        ("", "")
    } else {
        ("", rest)
    };
    let mut destination = String::new();
    if !scheme.is_empty() {
        destination.push_str(&scheme);
        destination.push(':');
    }
    if (!scheme.is_empty() || !host.is_empty()) && (!host.is_empty() || !path.is_empty()) {
        destination.push_str("//");
        destination.push_str(host);
    }
    if !path.is_empty() && !path.starts_with('/') && !host.is_empty() {
        destination.push('/');
    }
    destination.push_str(path);
    destination
}

/// Go's `textproto.CanonicalMIMEHeaderKey` for the token names `http.Header` holds.
fn canonical_header_key(name: &str) -> String {
    let mut upper = true;
    name.chars()
        .map(|c| {
            let mapped = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            mapped
        })
        .collect()
}

#[cfg(test)]
mod tests;
