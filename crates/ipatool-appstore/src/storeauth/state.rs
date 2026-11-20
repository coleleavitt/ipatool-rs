//! The persisted anisette state: Go's `anisetteState` and its `encoding/json` form.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{ANISETTE_LIMIT, base64_go};
use crate::{Error, Result};

/// Go's `anisetteState`: the software identity, the ADI database (`adi.pb`) and the routing
/// information Apple assigned. [`AnisetteState::to_json`] writes Go's `json.Marshal` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnisetteState {
    pub identifier: [u8; 16],
    /// `None` is Go's nil slice (`null`); `Some(vec![])` is an empty one (`""`).
    pub provisioning: Option<Vec<u8>>,
    pub routing_info: u64,
}

impl AnisetteState {
    /// Go's `json.Marshal(state)`.
    #[must_use]
    pub fn to_json(&self) -> Vec<u8> {
        let provisioning = self.provisioning.as_ref().map_or_else(
            || "null".to_owned(),
            |data| format!("\"{}\"", STANDARD.encode(data)),
        );
        format!(
            "{{\"Identifier\":\"{}\",\"Provisioning\":{provisioning},\"RoutingInfo\":{}}}",
            STANDARD.encode(self.identifier),
            self.routing_info
        )
        .into_bytes()
    }

    /// Go's `anisetteIdentityHeaders`: `X-Apple-I-MD-LU` (SHA-256 of the identity, hex) and
    /// `X-Mme-Device-Id` (the identity as a UUID).
    #[must_use]
    pub fn identity_headers(&self) -> (String, String) {
        let local_user_id = hex(&Sha256::digest(self.identifier));
        let encoded = hex(&self.identifier);
        let device_id = format!(
            "{}-{}-{}-{}-{}",
            &encoded[0..8],
            &encoded[8..12],
            &encoded[12..16],
            &encoded[16..20],
            &encoded[20..32]
        );
        (local_user_id, device_id)
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[usize::from(byte >> 4)],
                DIGITS[usize::from(byte & 15)],
            ]
        })
        .map(char::from)
        .collect()
}

/// Go's `decodeAnisetteState`: a fresh random identity for empty input, else the persisted
/// state with a 16-byte identity and at most 1 MiB of provisioning.
pub fn decode_anisette_state(encoded: &[u8]) -> Result<AnisetteState> {
    if encoded.is_empty() {
        let mut identifier = [0; 16];
        getrandom::fill(&mut identifier)
            .map_err(|error| Error::Invalid(format!("generate anisette identity: {error}")))?;
        return Ok(AnisetteState {
            identifier,
            provisioning: None,
            routing_info: 0,
        });
    }
    let invalid = || Error::Invalid("invalid persisted anisette state".to_owned());
    let raw = serde_json::from_slice::<RawState>(encoded).map_err(|_| invalid())?;
    let identifier = raw
        .identifier
        .as_deref()
        .and_then(|identifier| <[u8; 16]>::try_from(identifier).ok())
        .ok_or_else(invalid)?;
    if raw
        .provisioning
        .as_ref()
        .is_some_and(|data| data.len() > ANISETTE_LIMIT)
    {
        return Err(invalid());
    }
    Ok(AnisetteState {
        identifier,
        provisioning: raw.provisioning,
        routing_info: raw.routing_info,
    })
}

/// What `json.Unmarshal` fills in: keys match case-insensitively, the last one wins, unknown
/// keys are skipped, and `null` clears a slice but leaves the number alone.
#[derive(Default)]
struct RawState {
    identifier: Option<Vec<u8>>,
    provisioning: Option<Vec<u8>>,
    routing_info: u64,
}

impl<'de> Deserialize<'de> for RawState {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(RawStateVisitor)
    }
}

struct RawStateVisitor;

impl<'de> Visitor<'de> for RawStateVisitor {
    type Value = RawState;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an anisette state object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawState, A::Error> {
        let mut state = RawState::default();
        while let Some(key) = map.next_key::<String>()? {
            match fold_name(&key).as_str() {
                "IDENTIFIER" => state.identifier = go_bytes(map.next_value()?)?,
                "PROVISIONING" => state.provisioning = go_bytes(map.next_value()?)?,
                "ROUTINGINFO" => match map.next_value::<Value>()? {
                    Value::Null => {}
                    Value::Number(number) => {
                        state.routing_info = number
                            .as_u64()
                            .ok_or_else(|| de::Error::custom("RoutingInfo is not a uint64"))?;
                    }
                    _ => return Err(de::Error::custom("RoutingInfo is not a number")),
                },
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(state)
    }
}

/// A `[]byte` field: `null` or a standard-base64 string.
fn go_bytes<E: de::Error>(value: Value) -> Result<Option<Vec<u8>>, E> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) => base64_go::decode_valid(&text)
            .map(Some)
            .ok_or_else(|| E::custom("invalid base64")),
        _ => Err(E::custom("not a byte string")),
    }
}

/// `encoding/json`'s `foldName`: ASCII upper case, and the two non-ASCII runes whose simple
/// fold reaches an ASCII letter (`ſ`, the Kelvin sign).
fn fold_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\u{17f}' => 'S',
            '\u{212a}' => 'K',
            c => c.to_ascii_uppercase(),
        })
        .collect()
}
