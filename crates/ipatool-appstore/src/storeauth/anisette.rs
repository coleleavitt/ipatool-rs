//! Go's `AnisetteHeaders`: general machine provisioning with `gsa.apple.com` and a fresh
//! machine-validation proof (`anisette.go`).

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use time::OffsetDateTime;
use time::macros::format_description;

use super::http_client::{is_unknown_authority, read_limited};
use super::plist_xml::{self, Node};
use super::{ANISETTE_LIMIT, Adi, AnisetteState, base64_go, guest, header_value, transport};
use super::{AuthHttp, decode_anisette_state, open_fresh_adi};
use crate::{Error, Result};

const MACHINE_DSID: u64 = !1;
const MACHINE_START_URL: &str =
    "https://gsa.apple.com/grandslam/MidService/startMachineProvisioning";
const MACHINE_FINISH_URL: &str =
    "https://gsa.apple.com/grandslam/MidService/finishMachineProvisioning";
pub(super) const ANISETTE_CLIENT_INFO: &str =
    "<MacBookPro13,3> <Mac OS X;10.12.6;16G29> <com.apple.AuthKit/1 (com.apple.akd/1.0)>";
const ANISETTE_USER_AGENT: &str = "akd/1.0 CFNetwork/811.5.4 Darwin/16.7.0";

/// Go's `AnisetteHeaders`: provisions the persisted software identity with Apple and returns
/// the anisette headers with the updated state (Go's JSON). Empty `encoded` starts a new
/// identity.
pub async fn anisette_headers(client: &AuthHttp, encoded: &[u8]) -> Result<(HeaderMap, Vec<u8>)> {
    let mut state = decode_anisette_state(encoded)?;
    // Store provisioning replaces the ADI database. If its proof cannot be reused, start
    // general machine provisioning from the persisted identity.
    state.provisioning = None;
    state.routing_info = 0;
    let mut adi = open_fresh_adi(
        &state.identifier,
        "load anisette assets",
        "open anisette runtime",
    )
    .await?;
    machine_headers(
        client,
        state,
        &mut adi,
        MACHINE_START_URL,
        MACHINE_FINISH_URL,
    )
    .await
}

pub(super) async fn machine_headers(
    client: &AuthHttp,
    mut state: AnisetteState,
    adi: &mut impl Adi,
    start_url: &str,
    finish_url: &str,
) -> Result<(HeaderMap, Vec<u8>)> {
    provision_machine(client, &mut state, adi, start_url, finish_url).await?;
    let (mid, otp) = adi
        .request_otp(MACHINE_DSID)
        .map_err(guest("generate anisette proof"))?;
    state.provisioning = Some(adi.state().map_err(guest("read anisette state"))?);
    let headers = header_map(
        &state,
        &otp,
        &mid,
        OffsetDateTime::now_utc(),
        &go_local_zone(),
    )?;
    Ok((headers, state.to_json()))
}

async fn provision_machine(
    client: &AuthHttp,
    state: &mut AnisetteState,
    adi: &mut impl Adi,
    start_url: &str,
    finish_url: &str,
) -> Result<()> {
    let start = send_machine_provisioning(client, start_url, state, BTreeMap::new()).await?;
    let spim = decode_provisioning_field(&start, "spim")?;
    let (cpim, session) = adi
        .start_provisioning(MACHINE_DSID, &spim)
        .map_err(guest("start anisette provisioning"))?;
    let cpim = STANDARD.encode(cpim);
    let finish = send_machine_provisioning(
        client,
        finish_url,
        state,
        BTreeMap::from([("cpim", Node::String(&cpim))]),
    )
    .await?;
    let ptm = decode_provisioning_field(&finish, "ptm")?;
    let trust_key = decode_provisioning_field(&finish, "tk")?;
    let routing = provisioning_string(&finish, "X-Apple-I-MD-RINFO")?;
    state.routing_info = routing
        .parse::<u64>()
        .ok()
        .filter(|&routing| routing != 0)
        .ok_or_else(|| Error::Invalid("decode anisette routing information".to_owned()))?;
    adi.end_provisioning(session, &ptm, &trust_key)
        .map_err(guest("finish anisette provisioning"))
}

async fn send_machine_provisioning(
    client: &AuthHttp,
    endpoint: &str,
    state: &AnisetteState,
    values: BTreeMap<&str, Node<'_>>,
) -> Result<plist::Dictionary> {
    let body = plist_xml::encode(&BTreeMap::from([
        ("Header", Node::Dict(BTreeMap::new())),
        ("Request", Node::Dict(values)),
    ]));
    let (local_user_id, device_id) = state.identity_headers();
    let send = || {
        client
            .client()
            .post(endpoint)
            .header("Accept", "*/*")
            .header("Connection", "close")
            .header("Content-Type", "text/x-xml-plist")
            .header("User-Agent", ANISETTE_USER_AGENT)
            .header("X-Apple-I-MD-LU", &local_user_id)
            .header("X-MMe-Client-Info", ANISETTE_CLIENT_INFO)
            .header("X-Mme-Device-Id", &device_id)
            .body(body.clone())
            .send()
    };
    let response = match send().await {
        Err(error) if is_unknown_authority(&error) => {
            client
                .install_apple_root()
                .await
                .map_err(|error| error.context("install Apple provisioning root"))?;
            send().await
        }
        response => response,
    }
    .map_err(transport("send anisette provisioning request"))?;
    let status = response.status().as_u16();
    let data = read_limited(response, ANISETTE_LIMIT + 1)
        .await
        .map_err(transport("read anisette provisioning response"))?;
    if status != 200 || data.len() > ANISETTE_LIMIT {
        return Err(Error::Invalid(format!(
            "anisette provisioning returned HTTP {status}"
        )));
    }
    plist::from_bytes::<plist::Dictionary>(&data)
        .ok()
        .and_then(|envelope| match envelope.get("Response") {
            Some(plist::Value::Dictionary(response)) => Some(response.clone()),
            _ => None,
        })
        .ok_or_else(|| Error::Invalid("decode anisette provisioning response".to_owned()))
}

fn provisioning_string<'a>(values: &'a plist::Dictionary, key: &str) -> Result<&'a str> {
    values
        .get(key)
        .and_then(plist::Value::as_string)
        .filter(|value| header_value(value, 32768))
        .ok_or_else(|| Error::Invalid(format!("missing anisette {key}")))
}

fn decode_provisioning_field(values: &plist::Dictionary, key: &str) -> Result<Vec<u8>> {
    let value = provisioning_string(values, key)?;
    base64_go::decode_valid(value)
        .filter(|decoded| !decoded.is_empty() && decoded.len() <= ANISETTE_LIMIT)
        .ok_or_else(|| Error::Invalid(format!("decode anisette {key}")))
}

/// Go's `anisetteHeaders`.
pub(super) fn header_map(
    state: &AnisetteState,
    otp: &[u8],
    mid: &[u8],
    now: OffsetDateTime,
    zone: &str,
) -> Result<HeaderMap> {
    let (local_user_id, device_id) = state.identity_headers();
    let client_time = now
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
        ))
        .map_err(|error| Error::Invalid(format!("format anisette client time: {error}")))?;
    let values = [
        ("X-Apple-I-Client-Time", client_time),
        ("X-Apple-I-MD", STANDARD.encode(otp)),
        ("X-Apple-I-MD-M", STANDARD.encode(mid)),
        ("X-Apple-I-MD-RINFO", state.routing_info.to_string()),
        ("X-Apple-I-MD-LU", local_user_id),
        ("X-Apple-I-SRL-NO", "0".to_owned()),
        ("X-Apple-I-TimeZone", zone.to_owned()),
        ("X-Apple-Locale", "en_US".to_owned()),
        ("X-Mme-Device-Id", device_id),
        ("X-MMe-Client-Info", ANISETTE_CLIENT_INFO.to_owned()),
    ];
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        let invalid = || Error::Invalid(format!("invalid anisette header {name}"));
        let value = HeaderValue::try_from(value).map_err(|_| invalid())?;
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?,
            value,
        );
    }
    Ok(headers)
}

/// `time.Now().Location().String()` in Go: `Local` for the system zone, the `TZ` name when
/// it is set and loads, else `UTC`.
fn go_local_zone() -> String {
    if !cfg!(unix) {
        return "Local".to_owned();
    }
    match std::env::var("TZ") {
        Err(std::env::VarError::NotPresent) => if tzif(Path::new("/etc/localtime")) {
            "Local"
        } else {
            "UTC"
        }
        .to_owned(),
        Err(std::env::VarError::NotUnicode(_)) => "UTC".to_owned(),
        Ok(tz) => go_tz_name(&tz),
    }
}

fn go_tz_name(tz: &str) -> String {
    let tz = tz.strip_prefix(':').unwrap_or(tz);
    if tz.starts_with('/') {
        if !tzif(Path::new(tz)) {
            return "UTC".to_owned();
        }
        return if tz == "/etc/localtime" { "Local" } else { tz }.to_owned();
    }
    let sources = [
        "/usr/share/zoneinfo/",
        "/usr/share/lib/zoneinfo/",
        "/usr/lib/locale/TZ/",
        "/etc/zoneinfo",
    ];
    if !tz.is_empty()
        && tz != "UTC"
        && sources
            .iter()
            .any(|source| tzif(&Path::new(source).join(tz)))
    {
        return tz.to_owned();
    }
    "UTC".to_owned()
}

fn tzif(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|data| data.starts_with(b"TZif"))
}
