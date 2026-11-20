use std::sync::{Arc, Mutex};

use reqwest::header::HeaderMap;
use serde::Deserialize;
use time::macros::datetime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::anisette::{ANISETTE_CLIENT_INFO, header_map, machine_headers};
use super::store_provisioning::{StoreRequest, provision_store};
use super::*;

const IDENTIFIER: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

#[derive(Deserialize)]
struct Oracle {
    identities: Vec<OracleIdentity>,
    decodes: Vec<OracleDecode>,
    marshals: Vec<OracleDecode>,
    headers: Vec<OracleHeaders>,
    plists: Vec<OraclePlist>,
}

#[derive(Deserialize)]
struct OracleIdentity {
    identifier: String,
    local_user_id: String,
    device_id: String,
}

#[derive(Deserialize)]
struct OracleDecode {
    input: Option<String>,
    error: Option<String>,
    output: Option<String>,
}

#[derive(Deserialize)]
struct OracleHeaders {
    state: String,
    otp: String,
    mid: String,
    headers: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct OraclePlist {
    values: serde_json::Value,
    body: String,
}

fn oracle() -> Oracle {
    serde_json::from_str(include_str!("../../tests/fixtures/storeauth-oracle.json")).unwrap()
}

fn state(provisioning: Option<&[u8]>, routing_info: u64) -> AnisetteState {
    AnisetteState {
        identifier: IDENTIFIER,
        provisioning: provisioning.map(<[u8]>::to_vec),
        routing_info,
    }
}

#[test]
fn identity_headers_match_go() {
    for case in oracle().identities {
        let bytes: Vec<u8> = (0..32)
            .step_by(2)
            .map(|i| u8::from_str_radix(&case.identifier[i..i + 2], 16).unwrap())
            .collect();
        let state = AnisetteState {
            identifier: bytes.try_into().unwrap(),
            provisioning: None,
            routing_info: 0,
        };
        assert_eq!(
            state.identity_headers(),
            (case.local_user_id, case.device_id),
            "{}",
            case.identifier
        );
    }
}

#[test]
fn state_json_round_trips_like_go() {
    for case in oracle().decodes {
        let input = case.input.unwrap();
        match decode_anisette_state(input.as_bytes()) {
            Ok(state) => assert_eq!(
                String::from_utf8(state.to_json()).unwrap(),
                case.output.unwrap(),
                "{input}"
            ),
            Err(error) => assert_eq!(Some(error.to_string()), case.error, "{input}"),
        }
    }
    let marshals: Vec<String> = oracle()
        .marshals
        .into_iter()
        .filter_map(|m| m.output)
        .collect();
    let states = [
        state(None, 0),
        state(Some(&[]), 1),
        state(Some(b"\x00\xff state"), u64::MAX),
    ];
    for (state, expected) in states.iter().zip(&marshals) {
        assert_eq!(&String::from_utf8(state.to_json()).unwrap(), expected);
    }
}

#[test]
fn creates_a_fresh_software_identity() {
    let first = decode_anisette_state(b"").unwrap();
    assert_eq!(first.provisioning, None);
    assert_eq!(first.routing_info, 0);
    assert_ne!(
        first.identifier,
        decode_anisette_state(b"").unwrap().identifier
    );
}

#[test]
fn rejects_oversized_persisted_provisioning() {
    let encoded = state(Some(&vec![0; ANISETTE_LIMIT + 1]), 0).to_json();
    assert_eq!(
        decode_anisette_state(&encoded).unwrap_err().to_string(),
        "invalid persisted anisette state"
    );
    let at_limit = state(Some(&vec![0; ANISETTE_LIMIT]), 0).to_json();
    assert!(decode_anisette_state(&at_limit).is_ok());
}

#[test]
fn folds_field_names_like_go() {
    let decoded = decode_anisette_state(
        "{\"Identifier\":\"AAECAwQFBgcICQoLDA0ODw==\",\"PROVI\u{17f}IONING\":\"AA==\"}".as_bytes(),
    )
    .unwrap();
    assert_eq!(decoded.provisioning, Some(vec![0]));
}

#[test]
fn builds_the_complete_apple_header_set() {
    for case in oracle().headers {
        let state = decode_anisette_state(case.state.as_bytes()).unwrap();
        let now = datetime!(2026-11-20 07:08:09 UTC);
        let headers = header_map(
            &state,
            case.otp.as_bytes(),
            case.mid.as_bytes(),
            now,
            "Local",
        )
        .unwrap();
        let mut expected = case.headers.clone();
        expected.insert(
            "X-Apple-I-Client-Time".to_owned(),
            "2026-11-20T07:08:09Z".to_owned(),
        );
        expected.insert("X-Apple-I-Timezone".to_owned(), "Local".to_owned());
        assert_eq!(headers.len(), expected.len());
        for (name, value) in expected {
            assert_eq!(headers[name.as_str()], value.as_str(), "{name}");
        }
    }
    // The Go test's own assertions.
    let headers = header_map(
        &state(None, 123),
        b"otp",
        b"mid",
        datetime!(2026-01-01 0:00 UTC),
        "UTC",
    )
    .unwrap();
    assert_eq!(headers["X-Apple-I-MD"], "b3Rw");
    assert_eq!(headers["X-Apple-I-MD-M"], "bWlk");
    assert_eq!(headers["X-Apple-I-MD-RINFO"], "123");
    assert_eq!(
        headers["X-Mme-Device-Id"],
        "00010203-0405-0607-0809-0a0b0c0d0e0f"
    );
    assert_eq!(headers["X-MMe-Client-Info"], ANISETTE_CLIENT_INFO);
}

fn oracle_node(value: &serde_json::Value) -> plist_xml::Node<'_> {
    match value {
        serde_json::Value::String(text) => plist_xml::Node::String(text),
        serde_json::Value::Object(map) => plist_xml::Node::Dict(
            map.iter()
                .map(|(key, value)| (key.as_str(), oracle_node(value)))
                .collect(),
        ),
        other => panic!("unexpected oracle value {other}"),
    }
}

#[test]
fn request_plists_match_go() {
    for case in oracle().plists {
        let plist_xml::Node::Dict(root) = oracle_node(&case.values) else {
            panic!("oracle root is not a dictionary");
        };
        assert_eq!(
            String::from_utf8(plist_xml::encode(&root)).unwrap(),
            case.body
        );
    }
}

#[test]
fn store_amd_requires_provisioned_state() {
    let unprovisioned =
        "store AMD state is not provisioned; run 'ipatool auth login' to authenticate";
    for encoded in [
        Vec::new(),
        state(None, 1).to_json(),
        state(Some(&[]), 1).to_json(),
    ] {
        let error = EmulatorStoreAmd
            .headers(&encoded, "https://example", b"hw")
            .unwrap_err();
        assert_eq!(error.to_string(), unprovisioned);
    }
    let error = EmulatorStoreAmd.headers(b"{}", "", b"").unwrap_err();
    assert_eq!(error.to_string(), "invalid persisted anisette state");
    let state = state(Some(b"adi"), 1);
    assert_eq!(
        provisioned_store_amd_state(&state.to_json()).unwrap(),
        state
    );
}

/// A recorded request to the fake server.
#[derive(Debug)]
struct Request {
    head: String,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Vec<&str> {
        self.head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(": "))
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
            .collect()
    }
}

/// Answers one connection per scripted response, in order, and records each request.
async fn serve(responses: Vec<(u16, Vec<u8>)>) -> (String, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    tokio::spawn(async move {
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buffer = [0; 4096];
            let head_end = loop {
                let read = socket.read(&mut buffer).await.unwrap();
                data.extend_from_slice(&buffer[..read]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end;
                }
            };
            let head = String::from_utf8(data[..head_end].to_vec()).unwrap();
            let length: usize = head
                .lines()
                .filter_map(|line| line.split_once(": "))
                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, value)| value.parse().unwrap());
            while data.len() < head_end + 4 + length {
                let read = socket.read(&mut buffer).await.unwrap();
                data.extend_from_slice(&buffer[..read]);
            }
            let body_in = data[head_end + 4..].to_vec();
            recorded.lock().unwrap().push(Request {
                head,
                body: body_in,
            });
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    (base, requests)
}

fn plist(entries: &[(&str, &str)]) -> Vec<u8> {
    let dict: plist::Dictionary = entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), plist::Value::String((*value).to_owned())))
        .collect();
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &dict).unwrap();
    out
}

fn envelope(entries: &[(&str, &str)]) -> Vec<u8> {
    let response: plist::Dictionary = entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), plist::Value::String((*value).to_owned())))
        .collect();
    let mut dict = plist::Dictionary::new();
    dict.insert("Response".to_owned(), plist::Value::Dictionary(response));
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &dict).unwrap();
    out
}

/// Records the guest calls and answers with fixed values.
#[derive(Default)]
struct FakeAdi {
    calls: Vec<String>,
}

impl Adi for FakeAdi {
    fn start_provisioning(
        &mut self,
        dsid: u64,
        spim: &[u8],
    ) -> ipatool_sap::Result<(Vec<u8>, u32)> {
        self.calls
            .push(format!("start {dsid:#x} {}", String::from_utf8_lossy(spim)));
        Ok((b"cpim".to_vec(), 7))
    }

    fn end_provisioning(
        &mut self,
        session: u32,
        setting_info: &[u8],
        transport_key: &[u8],
    ) -> ipatool_sap::Result<()> {
        self.calls.push(format!(
            "end {session} {} {}",
            String::from_utf8_lossy(setting_info),
            String::from_utf8_lossy(transport_key)
        ));
        Ok(())
    }

    fn request_otp(&mut self, dsid: u64) -> ipatool_sap::Result<(Vec<u8>, Vec<u8>)> {
        self.calls.push(format!("otp {dsid:#x}"));
        Ok((b"mid".to_vec(), b"otp".to_vec()))
    }

    fn import_key_bag(&mut self, hardware_id: &[u8], key_bag: &[u8]) -> ipatool_sap::Result<()> {
        self.calls.push(format!(
            "keybag {} {}",
            String::from_utf8_lossy(hardware_id),
            String::from_utf8_lossy(key_bag)
        ));
        Ok(())
    }

    fn state(&mut self) -> ipatool_sap::Result<Vec<u8>> {
        Ok(b"adi.pb".to_vec())
    }
}

fn http() -> AuthHttp {
    AuthHttp::new(None).unwrap()
}

#[tokio::test]
async fn machine_provisioning_follows_apple_wire_order() {
    let (base, requests) = serve(vec![
        (200, envelope(&[("spim", "c3BpbQ==")])),
        (
            200,
            envelope(&[
                ("ptm", "cHRt"),
                ("tk", "dGs="),
                ("X-Apple-I-MD-RINFO", "17106176"),
            ]),
        ),
    ])
    .await;
    let mut adi = FakeAdi::default();
    let (headers, updated) = machine_headers(
        &http(),
        state(None, 0),
        &mut adi,
        &format!("{base}/start"),
        &format!("{base}/finish"),
    )
    .await
    .unwrap();

    assert_eq!(
        adi.calls,
        [
            "start 0xfffffffffffffffe spim",
            "end 7 ptm tk",
            "otp 0xfffffffffffffffe"
        ]
    );
    assert_eq!(
        String::from_utf8(updated).unwrap(),
        "{\"Identifier\":\"AAECAwQFBgcICQoLDA0ODw==\",\"Provisioning\":\"YWRpLnBi\",\"RoutingInfo\":17106176}"
    );
    assert_eq!(headers["X-Apple-I-MD"], "b3Rw");
    assert_eq!(headers["X-Apple-I-MD-M"], "bWlk");
    assert_eq!(headers["X-Apple-I-MD-RINFO"], "17106176");

    let requests = requests.lock().unwrap();
    let plists = oracle().plists;
    assert!(requests[0].head.starts_with("POST /start HTTP/1.1"));
    assert_eq!(requests[0].body, plists[0].body.as_bytes());
    assert!(requests[1].head.starts_with("POST /finish HTTP/1.1"));
    assert_eq!(String::from_utf8_lossy(&requests[1].body), plists[1].body);
    for request in requests.iter() {
        assert_eq!(request.header("accept"), ["*/*"]);
        assert_eq!(request.header("connection"), ["close"]);
        assert_eq!(request.header("content-type"), ["text/x-xml-plist"]);
        assert_eq!(
            request.header("user-agent"),
            ["akd/1.0 CFNetwork/811.5.4 Darwin/16.7.0"]
        );
        assert_eq!(
            request.header("x-apple-i-md-lu"),
            ["be45cb2605bf36bebde684841a28f0fd43c69850a3dce5fedba69928ee3a8991"]
        );
        assert_eq!(request.header("x-mme-client-info"), [ANISETTE_CLIENT_INFO]);
        assert_eq!(
            request.header("x-mme-device-id"),
            ["00010203-0405-0607-0809-0a0b0c0d0e0f"]
        );
    }
}

async fn machine_error(responses: Vec<(u16, Vec<u8>)>) -> String {
    let (base, _) = serve(responses).await;
    machine_headers(
        &http(),
        state(None, 0),
        &mut FakeAdi::default(),
        &format!("{base}/start"),
        &format!("{base}/finish"),
    )
    .await
    .unwrap_err()
    .to_string()
}

#[tokio::test]
async fn machine_provisioning_reports_go_errors() {
    assert_eq!(
        machine_error(vec![(500, envelope(&[("spim", "c3BpbQ==")]))]).await,
        "anisette provisioning returned HTTP 500"
    );
    assert_eq!(
        machine_error(vec![(200, vec![b'x'; ANISETTE_LIMIT + 1])]).await,
        "anisette provisioning returned HTTP 200"
    );
    assert_eq!(
        machine_error(vec![(200, plist(&[("spim", "c3BpbQ==")]))]).await,
        "decode anisette provisioning response"
    );
    assert_eq!(
        machine_error(vec![(200, envelope(&[]))]).await,
        "missing anisette spim"
    );
    assert_eq!(
        machine_error(vec![(200, envelope(&[("spim", "bad\u{1}")]))]).await,
        "missing anisette spim"
    );
    assert_eq!(
        machine_error(vec![(200, envelope(&[("spim", "c3BpbQ")]))]).await,
        "decode anisette spim"
    );
    let finish = |rinfo: &'static str| {
        vec![
            (200, envelope(&[("spim", "c3BpbQ==")])),
            (
                200,
                envelope(&[
                    ("ptm", "cHRt"),
                    ("tk", "dGs="),
                    ("X-Apple-I-MD-RINFO", rinfo),
                ]),
            ),
        ]
    };
    assert_eq!(
        machine_error(finish("0")).await,
        "decode anisette routing information"
    );
    assert_eq!(
        machine_error(finish("-1")).await,
        "decode anisette routing information"
    );
    assert_eq!(
        machine_error(vec![
            (200, envelope(&[("spim", "c3BpbQ==")])),
            (
                200,
                envelope(&[("ptm", "cHRt"), ("X-Apple-I-MD-RINFO", "1")])
            ),
        ])
        .await,
        "missing anisette tk"
    );
}

fn store_response(key_bag: Option<plist::Value>) -> Vec<u8> {
    let mut dict = plist::Dictionary::new();
    dict.insert("settingInfo".to_owned(), "c2V0dGluZw==".into());
    dict.insert("transportKey".to_owned(), "a2V5".into());
    if let Some(key_bag) = key_bag {
        dict.insert("keybag".to_owned(), key_bag);
    }
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &dict).unwrap();
    out
}

async fn store(
    responses: Vec<(u16, Vec<u8>)>,
    kbsync: &str,
    adi: &mut FakeAdi,
) -> (Result<Vec<u8>>, Vec<Request>) {
    let (base, requests) = serve(responses).await;
    let mut identity = HeaderMap::new();
    identity.insert("X-Apple-I-MD", "proof".parse().unwrap());
    identity.insert("x-apple-i-md-m", "mid".parse().unwrap());
    identity.insert("X-Apple-I-MD-RINFO", "1".parse().unwrap());
    identity.insert("X-Apple-I-MD-LU", "lu".parse().unwrap());
    identity.append("X-Extra", "one".parse().unwrap());
    identity.append("X-Extra", "two".parse().unwrap());
    identity.insert("User-Agent", "identity agent".parse().unwrap());
    let endpoint = format!("{base}/provision");
    let request = StoreRequest {
        endpoint: &endpoint,
        user_agent: "store agent",
        kbsync,
        identity: &identity,
    };
    let result = provision_store(&http(), state(None, 5), b"spim", adi, &request, b"hw").await;
    let requests = std::mem::take(&mut *requests.lock().unwrap());
    (result, requests)
}

#[tokio::test]
async fn store_provisioning_sends_go_request_and_imports_keybag() {
    let mut adi = FakeAdi::default();
    let (result, requests) = store(
        vec![(
            200,
            store_response(Some(plist::Value::Data(b"bag".to_vec()))),
        )],
        "a<b>&c\"d'e\tf\ng\rh",
        &mut adi,
    )
    .await;
    assert_eq!(
        String::from_utf8(result.unwrap()).unwrap(),
        "{\"Identifier\":\"AAECAwQFBgcICQoLDA0ODw==\",\"Provisioning\":\"YWRpLnBi\",\"RoutingInfo\":5}"
    );
    assert_eq!(
        adi.calls,
        [
            "start 0xffffffffffffffff spim",
            "end 7 setting key",
            "keybag hw bag"
        ]
    );
    let request = &requests[0];
    assert!(request.head.starts_with("POST /provision HTTP/1.1"));
    assert_eq!(
        String::from_utf8_lossy(&request.body),
        oracle().plists[3].body.replace("Y2xpZW50", "Y3BpbQ==")
    );
    assert_eq!(request.header("accept"), ["*/*"]);
    assert_eq!(
        request.header("content-type"),
        ["application/x-apple-plist"]
    );
    assert_eq!(request.header("user-agent"), ["store agent"]);
    assert_eq!(request.header("x-apple-i-md-lu"), ["lu"]);
    assert_eq!(request.header("x-extra"), ["one", "two"]);
    for skipped in ["x-apple-i-md", "x-apple-i-md-m", "x-apple-i-md-rinfo"] {
        assert!(request.header(skipped).is_empty(), "{skipped}");
    }
}

#[tokio::test]
async fn store_provisioning_reads_keybag_like_go() {
    let mut adi = FakeAdi::default();
    let (result, requests) = store(vec![(200, store_response(None))], "", &mut adi).await;
    result.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&requests[0].body),
        oracle().plists[2].body.replace("Y2xpZW50", "Y3BpbQ==")
    );
    assert_eq!(adi.calls.len(), 2, "no keybag import: {:?}", adi.calls);

    // A string keybag is base64; Go keeps the quanta decoded before corrupt input.
    let mut adi = FakeAdi::default();
    let corrupt = plist::Value::String("YmFnYmFn*".to_owned());
    store(vec![(200, store_response(Some(corrupt)))], "", &mut adi)
        .await
        .0
        .unwrap();
    assert_eq!(adi.calls[2], "keybag hw bagbag");
}

#[tokio::test]
async fn store_provisioning_reports_go_errors() {
    let error = |responses| async {
        store(responses, "", &mut FakeAdi::default())
            .await
            .0
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        error(vec![(403, store_response(None))]).await,
        "apple Store provisioning returned HTTP 403"
    );
    assert_eq!(
        error(vec![(200, b"not a plist".to_vec())]).await,
        "decode Apple Store provisioning response"
    );
    assert_eq!(
        error(vec![(200, plist(&[("transportKey", "a2V5")]))]).await,
        "apple Store provisioning response is missing settingInfo"
    );
    assert_eq!(
        error(vec![(200, plist(&[("settingInfo", "c2V0dGluZw==")]))]).await,
        "apple Store provisioning response is missing transportKey"
    );
    assert_eq!(
        error(vec![(
            200,
            plist(&[("settingInfo", "c2V0dGluZw"), ("transportKey", "a2V5")])
        )])
        .await,
        "decode Apple Store provisioning settingInfo"
    );
    assert_eq!(
        error(vec![(200, vec![b'x'; ANISETTE_LIMIT + 1])]).await,
        "read Apple Store provisioning response"
    );
}

#[tokio::test]
async fn store_provisioning_validates_challenge_before_loading_assets() {
    for challenge in ["", "  ", "c3Bp*", "c3BpbQ"] {
        let error = provision_store_anisette(
            &http(),
            &state(None, 0).to_json(),
            challenge,
            "http://127.0.0.1:9/",
            "agent",
            "",
            b"",
            &HeaderMap::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "decode Apple Store provisioning data",
            "{challenge:?}"
        );
    }
    let error = provision_store_anisette(
        &http(),
        b"{}",
        "c3BpbQ==",
        "",
        "",
        "",
        b"",
        &HeaderMap::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "invalid persisted anisette state");
}
