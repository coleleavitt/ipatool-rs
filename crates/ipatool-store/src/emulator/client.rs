//! Port of Go `internal/store/emulator/client.go`: one Store request through Apple's
//! encoder, session and response handling. The caller keeps session storage and package
//! assembly; encoding, protocol handling and response interpretation run in Apple code.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::auth::new_credential_task;
use super::bag::frozen_bag_from_values;
use super::bridge::import_value;
use super::delegate::interactive_delegate;
use super::runtime::{export, invoke};
use super::session::{
    configure_request, ephemeral_account, new_session, resolve_address, send_request,
};
use super::terms_adapter::install_terms_action_adapter;
use super::{Error, Guest, Payload, Result, Runtime, Uc, Value, codec, fail, go_float};
use crate::gourl;
use crate::http::{BoxError, Header, HttpResponse, Transport};

/// Signs a request body, as Go's `Sign func([]byte) ([]byte, error)`.
pub type Signer = Box<dyn FnMut(&[u8]) -> std::result::Result<Vec<u8>, BoxError>>;

/// Go's `emulator.Request`.
#[derive(Default)]
pub struct Request {
    /// Applies to transport I/O; the backend reads it when it builds the transport.
    pub timeout: Option<Duration>,
    pub url: String,
    pub method: String,
    pub operation: String,
    /// `json` re-encodes a decoded response as JSON; anything else as an XML plist.
    pub format: String,
    pub headers: Header,
    /// Go's `Parameters`: `None` is a nil map (a plain request), `Some` encodes them.
    pub parameters: Option<BTreeMap<String, Value>>,
    /// Go's `Body`: `None` is a nil slice.
    pub body: Option<Vec<u8>>,
    pub hardware_id: Vec<u8>,
    pub bag: Option<BTreeMap<String, Value>>,
    pub sign: Option<Signer>,
    pub transport: Option<Transport>,
    pub stream: bool,
    pub raw: bool,
    pub no_redirects: bool,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("url", &self.url)
            .field("method", &self.method)
            .field("operation", &self.operation)
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

/// Go's `fmt.Sprint` of a host value, as far as request parameters use it.
pub(crate) fn go_sprint(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Nil) => "<nil>".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Integer(number)) => number.to_string(),
        Some(Value::Real(number)) => go_float(*number),
        Some(Value::Boolean(flag)) => flag.to_string(),
        Some(Value::Data(bytes)) => format!(
            "[{}]",
            bytes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Some(Value::Date(date)) => Payload::Date(*date).sprint(),
        Some(Value::Array(items)) => format!(
            "[{}]",
            items
                .iter()
                .map(|item| go_sprint(Some(item)))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Some(Value::Dictionary(map)) => format!(
            "map[{}]",
            map.iter()
                .map(|(key, item)| format!("{key}:{}", go_sprint(Some(item))))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Some(Value::Other(name)) => format!("<{name}>"),
    }
}

/// Go's `allowedURL`.
pub(crate) fn allowed_url(value: &gourl::Url, authenticated: bool) -> bool {
    if value.user.is_some() || !value.fragment.is_empty() {
        return false;
    }
    let host = value.hostname().to_lowercase();
    if !authenticated {
        // Package and artwork URLs come from the Store response. Keep the downloader's
        // support for signed CDN URLs without binding it to one CDN.
        return !host.is_empty() && (value.scheme == "https" || value.scheme == "http");
    }
    let apple = host == "apple.com"
        || host.ends_with(".apple.com")
        || host.ends_with(".mzstatic.com")
        || host.ends_with(".cdn-apple.com");
    apple && value.scheme == "https"
}

fn new_string(uc: &mut Uc, text: impl Into<String>) -> Result<u64> {
    uc.new_object("NSString", Payload::String(text.into()))
}

/// Go's type-assertion panic inside `Execute`, recovered as a preparation error.
fn preparation(message: impl std::fmt::Display) -> Error {
    Error::msg(format!("Store request preparation: {message}"))
}

impl Runtime {
    /// Go's `Runtime.Execute`.
    ///
    /// # Errors
    /// Fails with the transport's error, a framework error without a finance response, or
    /// a guest or encoding failure.
    pub fn execute(&mut self, input: Request) -> Result<HttpResponse> {
        let result = execute(&mut self.uc, input);
        if result.is_err() && self.uc.st().network.stream_response {
            // Go closes a streamed body it will not return.
            self.uc.st().network.last_response = None;
        }
        result
    }
}

#[allow(clippy::too_many_lines, reason = "one request flow, as in Go")]
fn execute(uc: &mut Uc, mut input: Request) -> Result<HttpResponse> {
    let Some(transport) = input.transport.take() else {
        fail!("store transport is not configured");
    };
    if input.hardware_id.len() != 6 {
        fail!("invalid Store hardware identity");
    }
    let state = uc.st();
    state.hardware_id.clone_from(&input.hardware_id);
    state.network.transport = Some(transport);
    state.network.stream_response = input.stream;
    state.network.no_redirects = input.no_redirects;
    state.network.auth_request = input.operation == "authenticate";

    let dsid = input.headers.get("X-Dsid");
    let saved = (!dsid.is_empty()).then(|| {
        BTreeMap::from([
            (
                "directoryServicesIdentifier".to_owned(),
                Value::String(dsid.to_owned()),
            ),
            (
                "passwordToken".to_owned(),
                Value::String(input.headers.get("X-Token").to_owned()),
            ),
            (
                "storeFront".to_owned(),
                Value::String(input.headers.get("X-Apple-Store-Front").to_owned()),
            ),
            ("email".to_owned(), Value::String(String::new())),
        ])
    });
    let account = ephemeral_account(uc, saved.as_ref())?;

    let mut bag = 0;
    if let Some(values) = input.bag.as_ref().filter(|values| !values.is_empty()) {
        let mut values = values.clone();
        if let Some(Value::Dictionary(nested)) = values.get("urlBag").cloned() {
            values.extend(nested);
        }
        if input.operation == "purchase" {
            values.insert("buyProduct".to_owned(), Value::String(input.url.clone()));
        }
        bag = frozen_bag_from_values(uc, &values)?;
    }

    let session = new_session(uc)?;
    if input.operation != "authenticate" {
        let delegate = interactive_delegate(uc)?;
        invoke(uc, session, "setDelegate:", &[delegate])?;
    }
    if input.operation == "purchase" {
        install_terms_action_adapter(uc)?;
    }

    let mut parameters = input.parameters.clone();
    if input.operation == "authenticate" {
        if input.sign.is_none() {
            fail!("authentication requires a SAP signer");
        }
        let given = parameters.clone().unwrap_or_default();
        let username = match given.get("appleId") {
            Some(value) => import_value(uc, value)?,
            None => 0,
        };
        invoke(uc, account, "setUsername:", &[username])?;
        let password = go_sprint(given.get("password"));
        let task = new_credential_task(uc, account, bag, &password, 0)?;
        let value = invoke(
            uc,
            task,
            "_createAuthenticateRequestParametersForAccount:",
            &[account],
        )?;
        let built = export(uc, value, &mut std::collections::HashSet::new(), 0)?;
        let Value::Dictionary(mut native) = built else {
            return Err(preparation(format!(
                "interface conversion: interface {{}} is {}, not map[string]interface {{}}",
                go_type(&built)
            )));
        };
        let guid = go_sprint(native.get("guid"));
        if !guid.eq_ignore_ascii_case(&hex(&input.hardware_id)) {
            fail!("authentication hardware identity mismatch");
        }
        // Preserve the CLI's second credential attempt and 2FA retry contract.
        for key in ["attempt", "why"] {
            if let Some(value) = given.get(key) {
                native.insert(key.to_owned(), value.clone());
            }
        }
        parameters = Some(native);
        invoke(uc, session, "setDelegate:", &[task])?;
    }

    let request = if input.operation == "purchase" {
        purchase_request(uc, account, bag, &input)?
    } else if let Some(parameters) = &parameters {
        encode_request(uc, account, bag, &input.method, &input.url, parameters)?
    } else {
        let allocated = uc.new_object("AMSURLRequest", Payload::Nil)?;
        let url = uc.new_object("NSURL", Payload::String(input.url.clone()))?;
        let request = invoke(
            uc,
            allocated,
            "initWithURL:cachePolicy:timeoutInterval:",
            &[url, 0],
        )?;
        let method = new_string(uc, input.method.clone())?;
        invoke(uc, request, "setHTTPMethod:", &[method])?;
        if let Some(body) = &input.body {
            let data = uc.new_object("NSData", Payload::Data(body.clone()))?;
            invoke(uc, request, "setHTTPBody:", &[data])?;
        }
        request
    };

    configure_request(uc, request, account, bag, input.stream || input.raw)?;

    for (key, values) in input.headers.iter() {
        if key.eq_ignore_ascii_case("Content-Length") {
            continue;
        }
        let value = new_string(uc, values.join(", "))?;
        let name = new_string(uc, key)?;
        invoke(uc, request, "setValue:forHTTPHeaderField:", &[value, name])?;
    }

    if let Some(sign) = input.sign.as_mut() {
        let body = invoke(uc, request, "HTTPBody", &[])?;
        if body == 0 {
            fail!("signed request has no body");
        }
        let data = uc.data(body)?.clone();
        let signature =
            sign(&data).map_err(|err| Error::msg(format!("sign Store request: {err}")))?;
        let value = new_string(uc, STANDARD.encode(signature))?;
        let name = new_string(uc, "X-Apple-ActionSignature")?;
        invoke(uc, request, "setValue:forHTTPHeaderField:", &[value, name])?;
    }

    let result = send_request(uc, session, request);

    if let Some(err) = uc.st().network.transport_error.take() {
        return Err(Error::Transport(err));
    }
    let result = match result {
        Ok(result) => result,
        Err(err) => {
            // A decoded finance error is a real server response. Return its original body
            // to the compatibility layer for the existing 2FA and license sentinels.
            if err.framework().is_some()
                && !input.stream
                && let Some(response) = uc.st().network.last_response.take()
            {
                return Ok(response);
            }
            return Err(err);
        }
    };
    let Some(mut response) = uc.st().network.last_response.take() else {
        fail!("store session returned no HTTP response");
    };
    if input.stream || input.raw {
        return Ok(response);
    }
    let object = invoke(uc, result, "object", &[])?;
    if object != 0 {
        let value = export(uc, object, &mut std::collections::HashSet::new(), 0)?;
        let json = input.format == "json"
            || response
                .headers
                .get("Content-Type")
                .to_lowercase()
                .contains("json");
        let encoded = if json {
            codec::json(&value)
        } else {
            codec::plist_xml(&value)
        }
        .map_err(|err| Error::msg(format!("copy decoded Store response: {err}")))?;
        response.content_length = i64::try_from(encoded.len()).unwrap_or(i64::MAX);
        response.set_body(encoded);
        response.headers.del("Content-Encoding");
        response.headers.del("Content-Length");
    }
    Ok(response)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// Go's `%T` of an exported value.
fn go_type(value: &Value) -> &'static str {
    match value {
        Value::Nil => "nil",
        Value::String(_) => "string",
        Value::Integer(_) => "int64",
        Value::Real(_) => "float64",
        Value::Boolean(_) => "bool",
        Value::Data(_) => "[]uint8",
        Value::Date(_) => "time.Time",
        Value::Array(_) => "[]interface {}",
        Value::Dictionary(_) => "map[string]interface {}",
        Value::Other(name) => name,
    }
}

/// Go's `encodeRequest`: Apple's URL request encoder with the legacy client's policy.
fn encode_request(
    uc: &mut Uc,
    account: u64,
    bag: u64,
    method: &str,
    endpoint: &str,
    parameters: &BTreeMap<String, Value>,
) -> Result<u64> {
    let allocated = uc.new_object("AMSURLRequestEncoder", Payload::Nil)?;
    let encoder = invoke(uc, allocated, "initWithBag:", &[bag])?;
    // Go ranges over a map here, so its order is unspecified.
    for (selector, value) in [
        ("setAccount:", account),
        ("setRequestEncoding:", 2),
        ("setAnisetteType:", 0),
        ("setMescalType:", 0),
        ("setEnableRemoteSecuritySigning:", 0),
        ("setUrlKnownToBeTrusted:", 1),
        ("setShouldSetCookieHeader:", 0),
        ("setShouldSetStorefrontHeader:", 0),
        ("setDisableLoadURLMetrics:", 1),
    ] {
        invoke(uc, encoder, selector, &[value])?;
    }
    let method = match method {
        "GET" => 2,
        "POST" => 4,
        "PUT" => 5,
        _ => fail!("unsupported encoded Store request method"),
    };
    let url = uc.new_object("NSURL", Payload::String(endpoint.to_owned()))?;
    let parameters = import_value(uc, &Value::Dictionary(parameters.clone()))?;
    let promise = invoke(
        uc,
        encoder,
        "requestWithMethod:URL:parameters:",
        &[method, url, parameters],
    )?;
    resolve_address(uc, promise)
}

/// Go's `purchaseRequest`: free apps only, and the encoded product and price must be the
/// caller's.
fn purchase_request(uc: &mut Uc, account: u64, bag: u64, input: &Request) -> Result<u64> {
    if bag == 0 {
        fail!("purchase requires a Store bag");
    }
    let parameters = input.parameters.clone().unwrap_or_default();
    if go_sprint(parameters.get("price")) != "0"
        || parameters.get("productType") != Some(&Value::String("C".to_owned()))
    {
        fail!("purchasing paid apps is not supported");
    }
    let allocated = uc.new_object("AMSBuyParams", Payload::Nil)?;
    let params = invoke(uc, allocated, "init", &[])?;
    for (key, value) in &parameters {
        let value = import_value(uc, value)?;
        let key = new_string(uc, key.clone())?;
        invoke(uc, params, "setParameter:forKey:", &[value, key])?;
    }
    let allocated = uc.new_object("AMSPurchase", Payload::Nil)?;
    let purchase = invoke(
        uc,
        allocated,
        "initWithPurchaseType:buyParams:",
        &[0, params],
    )?;
    invoke(uc, purchase, "setAccount:", &[account])?;
    let allocated = uc.new_object("AMSPurchaseInfo", Payload::Nil)?;
    let info = invoke(uc, allocated, "initWithPurchase:", &[purchase])?;
    for (selector, value) in [("setAddKBSync:", 0), ("setHasBeenAuthedForBuy:", 1)] {
        invoke(uc, info, selector, &[value])?;
    }
    let encoder = super::purchase_adapter::configured_purchase_encoder(uc, info, bag)?;
    for (selector, value) in [
        ("setAnisetteType:", 0),
        ("setMescalType:", 0),
        ("setUrlKnownToBeTrusted:", 1),
        ("setEnableRemoteSecuritySigning:", 0),
        ("setDisableLoadURLMetrics:", 1),
        ("setCompressRequestBody:", 0),
        ("setShouldSetStorefrontHeader:", 0),
        ("setShouldSetCookieHeader:", 0),
    ] {
        invoke(uc, encoder, selector, &[value])?;
    }
    let promise = invoke(uc, encoder, "encodeRequest", &[])?;
    let request = resolve_address(uc, promise)?;
    let body = invoke(uc, request, "HTTPBody", &[])?;
    if body == 0 {
        fail!("purchase encoder returned no body");
    }
    let data = uc.data(body)?.clone();
    let Ok(Value::Dictionary(decoded)) = codec::parse_plist(&data) else {
        fail!("invalid encoded purchase");
    };
    for key in ["price", "salableAdamId", "pricingParameters", "productType"] {
        if go_sprint(decoded.get(key)) != go_sprint(parameters.get(key)) {
            fail!("purchase encoder changed product or price");
        }
    }
    Ok(request)
}

#[cfg(test)]
mod oracle_tests;
#[cfg(test)]
mod profile_tests;
#[cfg(test)]
mod tests;
