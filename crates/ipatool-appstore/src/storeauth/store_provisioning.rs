//! Go's `ProvisionStoreAnisette` (`store_provisioning.go`): the Store-specific ADI
//! provisioning action `auth.itunes.apple.com` returns during login.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};

use super::http_client::read_limited;
use super::plist_xml::{self, Node};
use super::{ANISETTE_LIMIT, Adi, AnisetteState, AuthHttp, base64_go, decode_anisette_state};
use super::{guest, header_value, open_fresh_adi, transport};
use crate::{Error, Result};

const STORE_ANISETTE_DSID: u64 = !0;
const MACHINE_HEADERS: [&str; 3] = ["X-Apple-I-MD", "X-Apple-I-MD-M", "X-Apple-I-MD-RINFO"];

/// Go's `ProvisionStoreAnisette`: answers Apple's Store provisioning `challenge` (base64 spim)
/// at `endpoint` with a fresh ADI database and returns the updated state (Go's JSON).
/// `identity` are the login request's headers; its machine proof headers are not forwarded.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors Go's ProvisionStoreAnisette signature"
)]
pub async fn provision_store_anisette(
    client: &AuthHttp,
    encoded: &[u8],
    challenge: &str,
    endpoint: &str,
    user_agent: &str,
    kbsync: &str,
    hardware_id: &[u8],
    identity: &HeaderMap,
) -> Result<Vec<u8>> {
    let state = decode_anisette_state(encoded)?;
    let spim = base64_go::decode_valid(challenge.trim())
        .filter(|spim| !spim.is_empty() && spim.len() <= ANISETTE_LIMIT)
        .ok_or_else(|| Error::Invalid("decode Apple Store provisioning data".to_owned()))?;
    let mut adi = open_fresh_adi(
        &state.identifier,
        "load local anisette assets",
        "open local anisette runtime",
    )
    .await?;
    let request = StoreRequest {
        endpoint,
        user_agent,
        kbsync,
        identity,
    };
    provision_store(client, state, &spim, &mut adi, &request, hardware_id).await
}

pub(super) struct StoreRequest<'a> {
    pub endpoint: &'a str,
    pub user_agent: &'a str,
    pub kbsync: &'a str,
    pub identity: &'a HeaderMap,
}

pub(super) async fn provision_store(
    client: &AuthHttp,
    mut state: AnisetteState,
    spim: &[u8],
    adi: &mut impl Adi,
    request: &StoreRequest<'_>,
    hardware_id: &[u8],
) -> Result<Vec<u8>> {
    let (cpim, session) = adi
        .start_provisioning(STORE_ANISETTE_DSID, spim)
        .map_err(guest("start Apple Store provisioning"))?;
    let client_data = STANDARD.encode(cpim);
    let mut payload = BTreeMap::from([("clientData", Node::String(&client_data))]);
    if !request.kbsync.is_empty() {
        payload.insert("kbsync", Node::String(request.kbsync));
    }
    let response = finish_store_provisioning(client, &payload, request).await?;
    let setting_info = decode_value(&response.setting_info, "settingInfo")?;
    let transport_key = decode_value(&response.transport_key, "transportKey")?;
    adi.end_provisioning(session, &setting_info, &transport_key)
        .map_err(|error| {
            Error::Sap(error.wrap(format!(
                "finish Apple Store provisioning (setting info {} bytes, transport key {} bytes)",
                setting_info.len(),
                transport_key.len()
            )))
        })?;
    if !response.key_bag.is_empty() {
        adi.import_key_bag(hardware_id, &response.key_bag)
            .map_err(guest("import Apple Store FairPlay keybag"))?;
    }
    state.provisioning = Some(
        adi.state()
            .map_err(guest("persist Apple Store provisioning"))?,
    );
    Ok(state.to_json())
}

struct StoreResponse {
    setting_info: String,
    transport_key: String,
    key_bag: Vec<u8>,
}

async fn finish_store_provisioning(
    client: &AuthHttp,
    payload: &BTreeMap<&str, Node<'_>>,
    request: &StoreRequest<'_>,
) -> Result<StoreResponse> {
    let mut headers = HeaderMap::new();
    for (name, value) in request.identity {
        if !MACHINE_HEADERS
            .iter()
            .any(|skip| name.as_str().eq_ignore_ascii_case(skip))
        {
            headers.append(name, value.clone());
        }
    }
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-apple-plist"),
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(request.user_agent)
            .map_err(|_| Error::Invalid("create Apple Store provisioning request".to_owned()))?,
    );
    let response = client
        .client()
        .post(request.endpoint)
        .headers(headers)
        .body(plist_xml::encode(payload))
        .send()
        .await
        .map_err(transport("send Apple Store provisioning request"))?;
    let status = response.status().as_u16();
    let read_error = || Error::Invalid("read Apple Store provisioning response".to_owned());
    let data = read_limited(response, ANISETTE_LIMIT + 1)
        .await
        .map_err(|_| read_error())?;
    if data.len() > ANISETTE_LIMIT {
        return Err(read_error());
    }
    if status != 200 {
        return Err(Error::Invalid(format!(
            "apple Store provisioning returned HTTP {status}"
        )));
    }
    let result = plist::from_bytes::<plist::Dictionary>(&data)
        .map_err(|_| Error::Invalid("decode Apple Store provisioning response".to_owned()))?;
    let string = |key: &str| {
        result
            .get(key)
            .and_then(plist::Value::as_string)
            .filter(|value| header_value(value, ANISETTE_LIMIT))
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "apple Store provisioning response is missing {key}"
                ))
            })
    };
    let setting_info = string("settingInfo")?;
    let transport_key = string("transportKey")?;
    // Go keeps a data keybag as is and decodes a string one, keeping what decoded before an error.
    let key_bag = match result.get("keybag") {
        Some(plist::Value::Data(data)) => data.clone(),
        Some(plist::Value::String(encoded)) => base64_go::decode(encoded).0,
        _ => Vec::new(),
    };
    if key_bag.len() > ANISETTE_LIMIT {
        return Err(Error::Invalid(
            "apple Store provisioning response contains an invalid keybag".to_owned(),
        ));
    }
    Ok(StoreResponse {
        setting_info,
        transport_key,
        key_bag,
    })
}

fn decode_value(value: &str, key: &str) -> Result<Vec<u8>> {
    base64_go::decode_valid(value)
        .filter(|decoded| !decoded.is_empty() && decoded.len() <= ANISETTE_LIMIT)
        .ok_or_else(|| Error::Invalid(format!("decode Apple Store provisioning {key}")))
}
