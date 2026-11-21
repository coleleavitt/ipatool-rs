//! Ports of Go's purchase and download tests (`pkg/appstore/*_test.go`). Store requests go
//! to a fake executor that records them and answers from a handler, as Go's mocked clients
//! and `downloadTestExecutor` do; nothing reaches Apple or the emulator.

#![allow(
    clippy::cast_possible_wrap,
    clippy::needless_pass_by_value,
    clippy::assert_is_empty,
    clippy::needless_raw_string_hashes,
    clippy::too_many_lines,
    clippy::single_match_else,
    clippy::comparison_to_empty,
    clippy::items_after_statements,
    reason = "test fixtures"
)]

mod download;
mod download_file;
mod download_product;
mod get_version_metadata;
mod kbsync;
mod list_versions;
mod partial_zip;
mod purchase;
mod redownload;
mod terms;
mod version_lookup;
mod zip;

use std::collections::{BTreeMap, VecDeque};
use std::io::Read;
use std::sync::{Arc, Mutex};

use ipatool_keychain::{Config, Keychain};
use plist::{Dictionary, Value as PValue};

use crate::commerce::{BoxError, CommerceRequest, Payload, StoreTransport};
use crate::storeauth::StoreAmd;
use crate::{Client, Header, HttpRequest, HttpResponse, Result, StoreSession, Value};

/// One recorded Store request.
#[derive(Clone, Debug)]
pub(crate) struct Sent {
    pub operation: String,
    pub request: CommerceRequest,
}

impl Sent {
    /// An XML payload parameter, as Go's `payload.Content[key]`.
    pub fn param(&self, key: &str) -> Option<&Value> {
        match &self.request.payload {
            Some(Payload::Xml(content)) => content.get(key),
            _ => None,
        }
    }

    pub fn param_str(&self, key: &str) -> &str {
        match self.param(key) {
            Some(Value::String(text)) => text,
            _ => "",
        }
    }

    pub fn header(&self, key: &str) -> &str {
        self.request.headers.get(key).map_or("", String::as_str)
    }
}

/// A canned executor response.
pub(crate) enum Reply {
    Plist(u16, Dictionary),
    Body(u16, Vec<(&'static str, String)>, Vec<u8>),
    Fail(&'static str),
}

pub(crate) fn plist(status: u16, entries: &[(&str, PValue)]) -> Reply {
    Reply::Plist(status, dict(entries))
}

pub(crate) fn dict(entries: &[(&str, PValue)]) -> Dictionary {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

pub(crate) fn s(text: &str) -> PValue {
    PValue::String(text.to_owned())
}

pub(crate) fn json(status: u16, body: &str) -> Reply {
    Reply::Body(status, Vec::new(), body.as_bytes().to_vec())
}

type Handler = Box<dyn Fn(&Sent) -> Reply + Send + Sync>;
pub(crate) type HttpHandler =
    Box<dyn Fn(&HttpRequest) -> std::result::Result<HttpResponse, BoxError> + Send + Sync>;

/// The fake Store executors.
pub(crate) struct Fake {
    pub sent: Mutex<Vec<Sent>>,
    queue: Mutex<VecDeque<Reply>>,
    handler: Mutex<Option<Handler>>,
    http: Mutex<Option<Arc<HttpHandler>>>,
    pub http_sent: Mutex<Vec<HttpRequest>>,
}

impl std::fmt::Debug for Fake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fake").finish_non_exhaustive()
    }
}

impl Fake {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::default(),
            queue: Mutex::default(),
            handler: Mutex::default(),
            http: Mutex::default(),
            http_sent: Mutex::default(),
        })
    }

    /// Answers the next requests in order.
    pub fn push(&self, reply: Reply) {
        self.queue.lock().unwrap().push_back(reply);
    }

    /// Answers requests the queue does not cover.
    pub fn handle(&self, handler: impl Fn(&Sent) -> Reply + Send + Sync + 'static) {
        *self.handler.lock().unwrap() = Some(Box::new(handler));
    }

    pub fn handle_http(
        &self,
        handler: impl Fn(&HttpRequest) -> std::result::Result<HttpResponse, BoxError>
        + Send
        + Sync
        + 'static,
    ) {
        *self.http.lock().unwrap() = Some(Arc::new(Box::new(handler)));
    }

    pub fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }

    /// The requests to `operation` whose URL contains `fragment`.
    pub fn sent_to(&self, fragment: &str) -> Vec<Sent> {
        self.sent()
            .into_iter()
            .filter(|sent| sent.request.url.contains(fragment))
            .collect()
    }
}

fn encode(dict: &Dictionary) -> Vec<u8> {
    let mut body = Vec::new();
    plist::to_writer_xml(&mut body, &PValue::Dictionary(dict.clone())).unwrap();
    body
}

impl StoreTransport for Fake {
    fn send(
        &self,
        operation: &str,
        request: CommerceRequest,
    ) -> std::result::Result<HttpResponse, BoxError> {
        let sent = Sent {
            operation: operation.to_owned(),
            request,
        };
        self.sent.lock().unwrap().push(sent.clone());
        let reply = self.queue.lock().unwrap().pop_front();
        let reply = match reply {
            Some(reply) => reply,
            None => match &*self.handler.lock().unwrap() {
                Some(handler) => handler(&sent),
                None => panic!("unexpected Store request {:?}", sent.request.url),
            },
        };
        match reply {
            Reply::Plist(status, dict) => {
                Ok(HttpResponse::new(status, Header::new(), encode(&dict)))
            }
            Reply::Body(status, headers, body) => {
                let mut header = Header::new();
                for (key, value) in headers {
                    header.add(key, value);
                }
                Ok(HttpResponse::new(status, header, body))
            }
            Reply::Fail(message) => Err(message.into()),
        }
    }

    fn http(
        &self,
        operation: &str,
        request: HttpRequest,
    ) -> std::result::Result<HttpResponse, BoxError> {
        assert_eq!(operation, "transfer");
        self.http_sent.lock().unwrap().push(request.clone());
        let handler = self.http.lock().unwrap().clone();
        match handler {
            Some(handler) => handler(&request),
            None => panic!("unexpected transfer {:?}", request.url),
        }
    }
}

/// Store AMD headers without the emulator; the state gains a byte per call.
#[derive(Default)]
pub(crate) struct FakeAmd {
    pub endpoints: Mutex<Vec<String>>,
}

impl StoreAmd for FakeAmd {
    fn headers(
        &self,
        state: &[u8],
        endpoint: &str,
        _hardware_id: &[u8],
    ) -> Result<(BTreeMap<String, String>, Vec<u8>)> {
        self.endpoints.lock().unwrap().push(endpoint.to_owned());
        let mut next = state.to_vec();
        next.push(b'x');
        Ok((
            BTreeMap::from([("X-Apple-AMD".to_owned(), "proof".to_owned())]),
            next,
        ))
    }
}

/// A file keychain in a temporary directory.
pub(crate) fn keychain(dir: &tempfile::TempDir) -> Keychain {
    Keychain::open_file(Config {
        service_name: "ipatool-test.service".to_owned(),
        id: None,
        file_dir: dir.path().to_owned(),
        passphrase: Box::new(|_| Ok("test".to_owned())),
    })
}

pub(crate) const GUID: &str = "001122334455";

/// The client, fake executors, keychain and Store AMD of one test.
pub(crate) struct World {
    pub client: Client,
    pub fake: Arc<Fake>,
    pub keychain: Keychain,
    pub amd: FakeAmd,
    pub dir: tempfile::TempDir,
}

impl World {
    pub fn new() -> Self {
        let fake = Fake::new();
        let client = Client::new()
            .unwrap()
            .with_store_transport(Arc::clone(&fake) as Arc<dyn StoreTransport>);
        client.cache_storefronts(&[("143441", "US"), ("143444", "GB")]);
        let dir = tempfile::tempdir().unwrap();
        Self {
            client,
            fake,
            keychain: keychain(&dir),
            amd: FakeAmd::default(),
            dir,
        }
    }

    /// A session with the machine at `00:11:22:33:44:55` and no kbsync generator.
    pub fn session(&self) -> StoreSession<'_> {
        let mut session = self.client.store_session(&self.keychain, &self.amd);
        session.kbsync_generator = None;
        session.mac_address = || Ok("00:11:22:33:44:55".to_owned());
        session
    }
}

/// A transfer response with an in-memory body.
pub(crate) fn transfer(status: u16, headers: &[(&str, String)], body: Vec<u8>) -> HttpResponse {
    let mut header = Header::new();
    for (key, value) in headers {
        header.add(key, value.clone());
    }
    HttpResponse::new(status, header, body)
}

/// A body that fails after `bytes`, as a dropped connection.
pub(crate) struct Truncated {
    pub data: std::io::Cursor<Vec<u8>>,
}

impl Read for Truncated {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.data.read(buffer)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset",
            ));
        }
        Ok(read)
    }
}
