//! Go's Store executors (`internal/store/backend.go`) as the App Store client uses them, and
//! the response handling of `pkg/http/client.go` for the clients that go through them.
//! Purchases, download requests, terms, catalog lookups and package transfers run in the
//! Store framework emulator, as in Go.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ipatool_store::backend::{Backend, MacAddress, ResponseFormat, StorePayload, StoreRequest};
use ipatool_store::emulator::Value;
use ipatool_store::http::{Header, HttpRequest, HttpResponse};
use serde::de::DeserializeOwned;

use crate::store::decode_xml_response;
use crate::{Client, CookieJar, Error, Result, TransportError, machine};

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Go's `apphttp.Payload` for the requests this crate sends.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// `XMLPayload`: parameters that Apple's encoder turns into the body.
    Xml(BTreeMap<String, Value>),
    /// `RawPayload`: the body as given.
    Raw(Vec<u8>),
}

/// Go's `apphttp.Request` for a Store executor. Unlike [`StoreRequest`] it is `Send`, so it
/// can move to the blocking thread the guest runs on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommerceRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub payload: Option<Payload>,
    pub response_format: ResponseFormat,
    pub no_redirects: bool,
    pub timeout: Option<Duration>,
    /// Go's `ActionSigner` result for the raw payload: the SAP signature of the body, or the
    /// signer's error. The signer runs Apple's code on the caller's thread and is not `Send`,
    /// so it signs before the request moves to the guest thread; the guest signs the body it
    /// was given unchanged, so the bytes are the same.
    pub signature: Option<std::result::Result<Vec<u8>, String>>,
}

/// Go's Store executors (`backend.Client(operation)`): `send` is `Executor.Send`, `http` is
/// `Executor.Do` (streamed for `transfer`). Both block.
pub trait StoreTransport: Send + Sync + fmt::Debug {
    /// # Errors
    /// The executor's error.
    fn send(&self, operation: &str, request: CommerceRequest) -> Result<HttpResponse, BoxError>;
    /// # Errors
    /// The executor's error.
    fn http(&self, operation: &str, request: HttpRequest) -> Result<HttpResponse, BoxError>;
}

/// The production executors: the Store framework emulator (Go's `store.New`). The backend
/// is built on first use, on the blocking thread that runs the request.
pub(crate) struct EmulatorTransport {
    jar: Option<Arc<CookieJar>>,
    backend: OnceLock<Arc<Backend>>,
}

impl fmt::Debug for EmulatorTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmulatorTransport").finish_non_exhaustive()
    }
}

impl EmulatorTransport {
    pub(crate) fn new(jar: Option<Arc<CookieJar>>) -> Self {
        Self {
            jar,
            backend: OnceLock::new(),
        }
    }

    fn backend(&self) -> Result<&Arc<Backend>, BoxError> {
        if let Some(backend) = self.backend.get() {
            return Ok(backend);
        }
        let jar = self
            .jar
            .clone()
            .map(|jar| jar as Arc<dyn reqwest::cookie::CookieStore>);
        let mac_address: MacAddress =
            Box::new(|| machine::mac_address().map_err(|error| Box::new(error) as BoxError));
        let backend = Arc::new(Backend::new(jar, Some(mac_address))?);
        Ok(self.backend.get_or_init(|| backend))
    }
}

impl StoreTransport for EmulatorTransport {
    fn send(&self, operation: &str, request: CommerceRequest) -> Result<HttpResponse, BoxError> {
        let (payload, body) = match request.payload {
            Some(Payload::Xml(content)) => (Some(StorePayload::Xml(Some(content))), Vec::new()),
            Some(Payload::Raw(body)) => (Some(StorePayload::Raw), body),
            None => (None, Vec::new()),
        };
        let request = StoreRequest {
            timeout: request.timeout,
            no_redirects: request.no_redirects,
            method: request.method,
            url: request.url,
            headers: request.headers,
            payload,
            action_signer: request.signature.map(|signature| {
                Box::new(move |_: &[u8]| signature.clone().map_err(BoxError::from))
                    as ipatool_store::emulator::Signer
            }),
            response_format: request.response_format,
        };
        Ok(self.backend()?.client(operation).send(request, body)?)
    }

    fn http(&self, operation: &str, request: HttpRequest) -> Result<HttpResponse, BoxError> {
        Ok(self.backend()?.client(operation).execute_http(request)?)
    }
}

/// Go's `http.Result` before decoding.
#[derive(Debug)]
pub struct StoreResponse {
    pub status: u16,
    pub headers: Header,
    pub body: Vec<u8>,
}

/// Go's `http.Result[R]`.
#[derive(Debug, Default)]
pub struct StoreResult<T> {
    pub status: u16,
    pub headers: Header,
    pub data: T,
}

fn transport_error(source: impl Into<BoxError>, retry_after: &str) -> Error {
    Error::Transport(TransportError {
        context: "request failed",
        source: source.into(),
        retry_after: retry_after.to_owned(),
    })
}

impl Client {
    /// Go's `client.Send` up to the body: runs the request through the `operation` executor
    /// on a blocking thread and saves the cookie jar.
    ///
    /// # Errors
    /// Go's `request failed: …` transport error, or the cookie jar's.
    pub async fn store_send(
        &self,
        operation: &'static str,
        request: CommerceRequest,
    ) -> Result<StoreResponse> {
        let transport = Arc::clone(&self.commerce);
        let jar = self.jar.clone();
        tokio::task::spawn_blocking(move || {
            let mut response = transport
                .send(operation, request)
                .map_err(|error| transport_error(error, ""))?;
            if let Some(jar) = &jar {
                jar.save()?;
            }
            let body = response.read_body().map_err(|error| {
                Error::Transport(TransportError {
                    context: "failed to read response body",
                    source: Box::new(error),
                    retry_after: response.headers.get("Retry-After").to_owned(),
                })
            })?;
            Ok(StoreResponse {
                status: response.status,
                headers: response.headers,
                body,
            })
        })
        .await
        .map_err(|error| Error::Invalid(format!("store request task failed: {error}")))?
    }

    /// Go's `Send` with `ResponseFormatXML` (`handleXMLResponse` without login diagnostics).
    /// Redirects keep `T::default()`, as Go leaves the result zero.
    ///
    /// # Errors
    /// See [`Client::store_send`]; HTML, empty and 429 responses with Go's text.
    pub async fn store_xml<T: DeserializeOwned + Default>(
        &self,
        operation: &'static str,
        mut request: CommerceRequest,
    ) -> Result<StoreResult<T>> {
        request.response_format = ResponseFormat::Xml;
        let response = self.store_send(operation, request).await?;
        let data = decode_xml_response(response.status, &response.body)?.unwrap_or_default();
        Ok(StoreResult {
            status: response.status,
            headers: response.headers,
            data,
        })
    }

    /// Go's `Send` with `ResponseFormatJSON`.
    ///
    /// # Errors
    /// See [`Client::store_send`]; `failed to unmarshal json: …`.
    pub async fn store_json<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        mut request: CommerceRequest,
    ) -> Result<StoreResult<T>> {
        request.response_format = ResponseFormat::Json;
        let response = self.store_send(operation, request).await?;
        let data = serde_json::from_slice(&response.body)
            .map_err(|error| Error::Invalid(format!("failed to unmarshal json: {error}")))?;
        Ok(StoreResult {
            status: response.status,
            // Go's JSON results carry no headers.
            headers: Header::new(),
            data,
        })
    }

    /// Go's `Send` with `ResponseFormatRaw`.
    ///
    /// # Errors
    /// See [`Client::store_send`].
    pub async fn store_raw(
        &self,
        operation: &'static str,
        mut request: CommerceRequest,
    ) -> Result<StoreResult<Vec<u8>>> {
        request.response_format = ResponseFormat::Raw;
        let response = self.store_send(operation, request).await?;
        Ok(StoreResult {
            status: response.status,
            headers: response.headers,
            data: response.body,
        })
    }

    /// Go's `client.Do`: a prepared request through the `operation` executor. Blocks; a
    /// `transfer` response streams its body.
    ///
    /// # Errors
    /// `received error: …` with the executor's error, as Go.
    pub fn store_http(&self, operation: &str, request: HttpRequest) -> Result<HttpResponse> {
        self.commerce
            .http(operation, request)
            .map_err(|error| Error::Invalid(format!("received error: {error}")))
    }
}
