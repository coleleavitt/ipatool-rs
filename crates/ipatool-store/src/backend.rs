//! Port of Go `internal/store/backend.go`: the framework-backed App Store request engine.
//! Each request runs in its own guest; the backend shares only cookies, the bag and the
//! HTTP connection pools.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use reqwest::cookie::CookieStore;

use crate::emulator::{Request, Runtime, Signer, Value, codec, valid_method};
use crate::http::{BoxError, Header, HttpRequest, HttpResponse, Transport};
use crate::{plist_body, profile};

/// Go's `http.DefaultTransport` dial keep-alive and the backend's idle timeout.
const KEEP_ALIVE: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Go's default for `api` requests without their own timeout.
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// The user agent Go's HTTP client sends when a request sets none.
const GO_USER_AGENT: &str = "Go-http-client/1.1";

/// Reads the machine's MAC address, as Go's `macAddress` callback.
pub type MacAddress = Box<dyn Fn() -> Result<String, BoxError> + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("store HTTP transport: {0}")]
    Transport(#[source] BoxError),
    #[error("create Store HTTP client: {0}")]
    Client(#[source] reqwest::Error),
}

/// reqwest wants a sized cookie provider.
struct SharedJar(Arc<dyn CookieStore>);

impl CookieStore for SharedJar {
    fn set_cookies(
        &self,
        cookie_headers: &mut dyn Iterator<Item = &reqwest::header::HeaderValue>,
        url: &url::Url,
    ) {
        self.0.set_cookies(cookie_headers, url);
    }

    fn cookies(&self, url: &url::Url) -> Option<reqwest::header::HeaderValue> {
        self.0.cookies(url)
    }
}

struct Pools {
    transport: Client,
    auth_transport: Client,
}

/// Go's `store.Backend`.
pub struct Backend {
    jar: Option<Arc<dyn CookieStore>>,
    mac_address: Option<MacAddress>,
    pools: Mutex<Pools>,
    image: Mutex<Option<Arc<Vec<u8>>>>,
    bag: Mutex<Option<BTreeMap<String, Value>>>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("jar", &self.jar.is_some())
            .field("mac_address", &self.mac_address.is_some())
            .finish_non_exhaustive()
    }
}

fn pools(jar: Option<&Arc<dyn CookieStore>>) -> Result<Pools, BackendError> {
    // Redirects go back to the framework, which follows them itself; cookies are the
    // backend's jar, read only when a request has no `Cookie` header (as in Go).
    let builder = || {
        let builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(None)
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_keepalive(KEEP_ALIVE)
            .pool_idle_timeout(IDLE_TIMEOUT);
        match jar {
            Some(jar) => builder.cookie_provider(Arc::new(SharedJar(Arc::clone(jar)))),
            None => builder,
        }
    };
    Ok(Pools {
        transport: builder().build().map_err(BackendError::Client)?,
        // Go's authentication transport disables keep-alives.
        auth_transport: builder()
            .pool_max_idle_per_host(0)
            .build()
            .map_err(BackendError::Client)?,
    })
}

impl Backend {
    /// Go's `store.New`.
    ///
    /// # Errors
    /// Fails when the TLS backend cannot be initialized.
    pub fn new(
        jar: Option<Arc<dyn CookieStore>>,
        mac_address: Option<MacAddress>,
    ) -> Result<Self, BackendError> {
        let pools = pools(jar.as_ref())?;
        Ok(Self {
            jar,
            mac_address,
            pools: Mutex::new(pools),
            image: Mutex::new(None),
            bag: Mutex::new(None),
        })
    }

    /// Go's `CloseIdleConnections`: releases pooled sockets without interrupting active
    /// streams, which own their connection.
    ///
    /// # Errors
    /// Fails when the replacement pools cannot be built.
    pub fn close_idle_connections(&self) -> Result<(), BackendError> {
        let fresh = pools(self.jar.as_ref())?;
        *self.pools.lock().unwrap_or_else(PoisonError::into_inner) = fresh;
        Ok(())
    }

    /// Go's `requestTransport`: each request owns its timeout; compatible requests share
    /// only the connection pool. Login keeps a fresh connection per request, including
    /// framework redirects.
    pub(crate) fn request_transport(
        &self,
        operation: &str,
        timeout: Option<Duration>,
    ) -> Transport {
        let timeout = timeout.or((operation == "api").then_some(API_TIMEOUT));
        let authenticate = operation == "authenticate";
        let pools = self.pools.lock().unwrap_or_else(PoisonError::into_inner);
        let client = if authenticate {
            pools.auth_transport.clone()
        } else {
            pools.transport.clone()
        };
        drop(pools);
        Box::new(move |request| {
            send(&client, request, timeout, authenticate)
                .map_err(|err| Box::new(BackendError::Transport(err)) as BoxError)
        })
    }
}

/// Go's `http.Client.Do` over the backend transport, with Go's implicit request headers
/// and transparent gzip.
fn send(
    client: &Client,
    request: HttpRequest,
    timeout: Option<Duration>,
    close: bool,
) -> Result<HttpResponse, BoxError> {
    let HttpRequest {
        method,
        url,
        mut headers,
        body,
    } = request;
    let method = reqwest::Method::from_bytes(method.as_bytes())?;
    let mut builder = client.request(method.clone(), &url);
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    if headers.get("User-Agent").is_empty() {
        headers.set("User-Agent", GO_USER_AGENT);
    }
    let gzip = headers.get("Accept-Encoding").is_empty()
        && headers.get("Range").is_empty()
        && method != reqwest::Method::HEAD;
    if gzip {
        headers.set("Accept-Encoding", "gzip");
    }
    if close && headers.get("Connection").is_empty() {
        headers.set("Connection", "close");
    }
    for (key, values) in headers.iter() {
        for value in values {
            builder = builder.header(key, value);
        }
    }
    if !body.is_empty() {
        builder = builder.body(body);
    }
    let response = builder.send()?;
    let status = response.status().as_u16();
    let mut headers = Header::from_wire(response.headers());
    let mut content_length = response
        .content_length()
        .and_then(|length| i64::try_from(length).ok())
        .unwrap_or(-1);
    let body: Box<dyn Read + Send> =
        if gzip && headers.get("Content-Encoding").eq_ignore_ascii_case("gzip") {
            headers.del("Content-Encoding");
            headers.del("Content-Length");
            content_length = -1;
            Box::new(GzDecoder::new(response))
        } else {
            Box::new(response)
        };
    Ok(HttpResponse {
        status,
        headers,
        body,
        content_length,
        request_url: Some(url),
    })
}

/// Go's `net.ParseMAC`, as far as the backend accepts it: six-byte addresses in the
/// colon, hyphen or dot forms. Longer EUI-64 and InfiniBand forms parse in Go but are
/// rejected by the caller, so they are rejected here too.
pub(crate) fn parse_mac(value: &str) -> Option<[u8; 6]> {
    let bytes = value.as_bytes();
    let mut mac = [0; 6];
    let hex = |pair: &[u8]| -> Option<u8> {
        let text = std::str::from_utf8(pair).ok()?;
        if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u8::from_str_radix(text, 16).ok()
    };
    if bytes.len() == 17 && (bytes[2] == b':' || bytes[2] == b'-') {
        let separator = bytes[2];
        for (index, byte) in mac.iter_mut().enumerate() {
            let start = index * 3;
            if index < 5 && bytes[start + 2] != separator {
                return None;
            }
            *byte = hex(&bytes[start..start + 2])?;
        }
        return Some(mac);
    }
    if bytes.len() == 14 && bytes[4] == b'.' {
        for (group, pair) in mac.chunks_mut(2).enumerate() {
            let start = group * 5;
            if group < 2 && bytes[start + 4] != b'.' {
                return None;
            }
            pair[0] = hex(&bytes[start..start + 2])?;
            pair[1] = hex(&bytes[start + 2..start + 4])?;
        }
        return Some(mac);
    }
    None
}

/// Go's `apphttp.DefaultUserAgent`.
pub const DEFAULT_USER_AGENT: &str =
    "Configurator/2.17 (Macintosh; OS X 15.2; 24C5089c) AppleWebKit/0620.1.16.11.6";
/// The user agent Go sends purchases with.
const PURCHASE_USER_AGENT: &str =
    "Configurator/2.18 (Macintosh; OS X 15.3.2; 24D81) AppleWebKit/0620.2.4.11.6";
/// Go's limits on a request body read by `Do` and on a decompressed response.
const REQUEST_LIMIT: usize = 1 << 20;
const RESPONSE_LIMIT: u64 = 32 << 20;

/// Go's `apphttp.ResponseFormat`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResponseFormat {
    #[default]
    Unspecified,
    Json,
    Xml,
    Raw,
}

impl ResponseFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "",
            Self::Json => "json",
            Self::Xml => "xml",
            Self::Raw => "raw",
        }
    }
}

/// Go's `apphttp.Payload`.
#[derive(Clone, Debug, PartialEq)]
pub enum StorePayload {
    /// `XMLPayload`: the parameters Apple's encoder turns into the body. `None` is Go's nil
    /// `Content`, sent as an empty dictionary.
    Xml(Option<BTreeMap<String, Value>>),
    /// `RawPayload`: the body is sent as given.
    Raw,
}

/// Go's `apphttp.Request` as the Store executor reads it.
#[derive(Default)]
pub struct StoreRequest {
    /// Applies to transport I/O after runtime asset preparation.
    pub timeout: Option<Duration>,
    pub no_redirects: bool,
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub payload: Option<StorePayload>,
    pub action_signer: Option<Signer>,
    pub response_format: ResponseFormat,
}

/// Go's `store.executor`: the backend bound to one operation (`request`, `sap`,
/// `authenticate`, `purchase`, `bag`, `bag-direct`, `transfer`, `api`).
#[derive(Clone, Debug)]
pub struct Executor {
    backend: Arc<Backend>,
    operation: String,
}

/// A backend failure with Go's text.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Go's `fmt.Errorf("context: %w", err)`.
    #[error("{context}: {source}")]
    Wrapped {
        context: &'static str,
        #[source]
        source: BoxError,
    },
    /// Go's `errors.New(text)`.
    #[error("{0}")]
    Message(&'static str),
    /// The transport's own error, returned unwrapped as in Go.
    #[error(transparent)]
    Transport(BoxError),
}

impl StoreError {
    fn new(context: &'static str, source: impl Into<BoxError>) -> Self {
        Self::Wrapped {
            context,
            source: source.into(),
        }
    }
}

impl Backend {
    /// Go's `Client(operation)`: shares session cookies and bag data while isolating each
    /// guest VM.
    #[must_use]
    pub fn client(self: &Arc<Self>, operation: &str) -> Executor {
        Executor {
            backend: Arc::clone(self),
            operation: operation.to_owned(),
        }
    }

    /// Go's `hardware`: the request's `guid` parameter, else the machine's MAC address.
    fn hardware(&self, parameters: Option<&BTreeMap<String, Value>>) -> Result<Vec<u8>, BoxError> {
        if let Some(Value::String(guid)) = parameters.and_then(|p| p.get("guid")) {
            return decode_hex(guid)
                .filter(|data| data.len() == 6)
                .ok_or_else(|| StoreError::Message("invalid Store hardware identity").into());
        }
        let Some(mac_address) = &self.mac_address else {
            return Err(StoreError::Message("store machine identity is not configured").into());
        };
        let value =
            mac_address().map_err(|err| StoreError::new("get Store machine identity", err))?;
        parse_mac(&value)
            .map(|mac| mac.to_vec())
            .ok_or_else(|| StoreError::Message("invalid Store machine address").into())
    }

    /// Go's `prepare`: the pinned framework image (cached) and the embedded profile.
    fn prepare(&self) -> Result<(Arc<Vec<u8>>, &'static profile::Data), BoxError> {
        let prepared = profile::load()
            .map_err(|err| StoreError::new("load Store runtime", err.to_string()))?;
        let mut image = self.image.lock().unwrap_or_else(PoisonError::into_inner);
        if image.is_none() {
            let bundle = ipatool_sap::assets::load()
                .map_err(|err| StoreError::new("load Store runtime", err))?;
            *image = Some(Arc::new(bundle.apple_media_services));
        }
        let image = image
            .clone()
            .ok_or_else(|| StoreError::Message("load Store runtime"))?;
        Ok((image, prepared))
    }

    /// Go's `execute`: one request in a fresh guest.
    ///
    /// # Errors
    /// Fails with Go's wrapped request, preparation and response errors.
    #[allow(clippy::too_many_lines, reason = "one request flow, as in Go")]
    pub fn execute(&self, mut input: Request) -> Result<HttpResponse, StoreError> {
        let hardware = self
            .hardware(input.parameters.as_ref())
            .map_err(|err| StoreError::new("store request", err))?;
        input.hardware_id.clone_from(&hardware);
        if input.headers.get("User-Agent").is_empty() {
            input.headers.set("User-Agent", DEFAULT_USER_AGENT);
            if input.operation == "purchase" {
                input.headers.set("User-Agent", PURCHASE_USER_AGENT);
            }
        }
        let mut bag = self
            .bag
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if input.operation == "purchase" && bag.is_none() {
            let url = format!(
                "https://init.itunes.apple.com/bag.xml?guid={}",
                hex_upper(&hardware)
            );
            self.execute(Request {
                url,
                method: "GET".to_owned(),
                operation: "bag".to_owned(),
                ..Request::default()
            })
            .map_err(|err| StoreError::new("load purchase bag", err))?;
            bag.clone_from(&self.bag.lock().unwrap_or_else(PoisonError::into_inner));
        }
        input.bag = bag;
        let (image, prepared) = self
            .prepare()
            .map_err(|err| StoreError::new("prepare Store framework", err))?;
        let mut guest = Runtime::open(&image, prepared)
            .map_err(|err| StoreError::new("open Store framework", err))?;
        input.transport = Some(self.request_transport(&input.operation, input.timeout));
        let (stream, is_bag) = (
            input.stream,
            input.url.contains("init.itunes.apple.com/bag.xml"),
        );
        let mut response = guest
            .execute(input)
            .map_err(|err| StoreError::new("store request", err))?;
        drop(guest);
        if stream {
            return Ok(response);
        }
        if response
            .headers
            .get("Content-Encoding")
            .eq_ignore_ascii_case("gzip")
        {
            let body = std::mem::replace(&mut response.body, Box::new(std::io::empty()));
            let mut reader = GzDecoder::new(body);
            let mut data = Vec::new();
            let read = (&mut reader)
                .take(RESPONSE_LIMIT + 1)
                .read_to_end(&mut data);
            // Go's gzip.NewReader fails on a missing or bad header before any read.
            if reader.header().is_none() {
                return Err(StoreError::Message("invalid compressed Store response"));
            }
            read.map_err(|err| StoreError::new("read compressed Store response", err))?;
            if data.len() as u64 > RESPONSE_LIMIT {
                return Err(StoreError::Message("store response exceeds size limit"));
            }
            response.headers.del("Content-Encoding");
            response.headers.del("Content-Length");
            response.content_length = i64::try_from(data.len()).unwrap_or(i64::MAX);
            response.set_body(data);
        }
        if is_bag && response.status == 200 {
            let data = response
                .read_body()
                .map_err(|err| StoreError::new("read Store response", err))?;
            if let Ok(plist::Value::Dictionary(values)) = plist_body::decode::<plist::Value>(&data)
                && let Value::Dictionary(values) =
                    codec::from_plist(plist::Value::Dictionary(values))
            {
                *self.bag.lock().unwrap_or_else(PoisonError::into_inner) = Some(values);
            }
            response.set_body(data);
        }
        Ok(response)
    }
}

impl Executor {
    /// Go's `executor.Send`: a Store API request whose payload Apple's encoder builds.
    ///
    /// # Errors
    /// As [`Backend::execute`]; `bag-direct` fails with the transport error.
    pub fn send(&self, request: StoreRequest, body: Vec<u8>) -> Result<HttpResponse, StoreError> {
        let mut headers = Header::new();
        for (key, value) in &request.headers {
            headers.set(key, value.clone());
        }
        if self.operation == "bag-direct" {
            if !valid_method(&request.method) {
                return Err(StoreError::new(
                    "create Store request",
                    format!("net/http: invalid method {:?}", request.method),
                ));
            }
            if let Err(err) = crate::gourl::parse(&request.url) {
                return Err(StoreError::new("create Store request", err));
            }
            if headers.get("User-Agent").is_empty() {
                headers.set("User-Agent", DEFAULT_USER_AGENT);
            }
            let method = if request.method.is_empty() {
                "GET".to_owned()
            } else {
                request.method
            };
            let direct = HttpRequest {
                method,
                url: request.url,
                headers,
                body,
            };
            return (self
                .backend
                .request_transport("authenticate", request.timeout))(direct)
            .map_err(StoreError::Transport);
        }
        let raw = request.response_format == ResponseFormat::Raw;
        let parameters = match request.payload {
            Some(StorePayload::Xml(content)) => Some(content.unwrap_or_default()),
            _ => None,
        };
        self.backend.execute(Request {
            url: request.url,
            method: request.method,
            operation: self.operation.clone(),
            format: request.response_format.as_str().to_owned(),
            timeout: request.timeout,
            body: Some(body),
            headers,
            no_redirects: request.no_redirects,
            raw,
            parameters,
            sign: request.action_signer,
            ..Request::default()
        })
    }

    /// Go's `executor.Do`: a prepared HTTP request (SAP setup, downloads). An empty body
    /// is Go's nil body.
    ///
    /// # Errors
    /// As [`Backend::execute`]; `sap` fails with the transport error.
    pub fn execute_http(&self, request: HttpRequest) -> Result<HttpResponse, StoreError> {
        if self.operation == "sap" {
            return (self.backend.request_transport("authenticate", None))(request)
                .map_err(StoreError::Transport);
        }
        if request.body.len() > REQUEST_LIMIT {
            return Err(StoreError::Message("store request body exceeds limit"));
        }
        let body = (!request.body.is_empty()).then_some(request.body);
        self.backend.execute(Request {
            url: request.url,
            method: request.method,
            headers: request.headers,
            body,
            stream: self.operation == "transfer",
            operation: self.operation.clone(),
            ..Request::default()
        })
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| {
            text.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect()
}

fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02X}");
        text
    })
}

#[cfg(test)]
mod tests;
