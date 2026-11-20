//! Go `internal/store/profile/format`: the portable Store runtime profile, with Go's JSON
//! field names.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

type Addresses = BTreeMap<String, u64>;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Method {
    pub selector: String,
    pub address: u64,
    pub class_method: bool,
    pub types: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Class {
    pub meta_address: u64,
    pub name: String,
    pub address: u64,
    #[serde(rename = "Super")]
    pub superclass: String,
    pub size: u64,
    pub ivars: Option<Addresses>,
    pub methods: Option<Vec<Method>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Call {
    pub address: u64,
    pub symbol: String,
    pub selector: String,
    pub selector_address: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Page {
    pub address: u64,
    pub offset: u64,
}

/// Go's `Constant`; the `omitempty` fields default when absent.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Constant {
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_boolean: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub boolean: bool,
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub number: i64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entries: Addresses,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<u64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct DictionaryLayout {
    pub metadata: u64,
    pub key_stride: u64,
    pub value_stride: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct MetadataPatch {
    pub address: u64,
    /// Go encodes `[]byte` as standard base64.
    #[serde(serialize_with = "to_base64", deserialize_with = "from_base64")]
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Profile {
    pub bridges: Addresses,
    #[serde(rename = "ObjCMetadata")]
    pub objc_metadata: Addresses,
    pub swift_singletons: Addresses,
    pub metadata_patches: Vec<MetadataPatch>,
    pub swift_foreign: Addresses,
    #[serde(rename = "PluginUUID")]
    pub plugin_uuid: String,
    pub swift_dictionaries: BTreeMap<String, DictionaryLayout>,
    pub swift_globals: Addresses,
    pub swift_types: Addresses,
    pub swift_witnesses: Addresses,
    pub swift_accessors: Addresses,
    pub symbols: Addresses,
    pub features: BTreeMap<String, bool>,
    pub constants: BTreeMap<String, Constant>,
    #[serde(rename = "UUID")]
    pub uuid: String,
    pub classes: Vec<Class>,
    pub calls: Vec<Call>,
    pub strings: BTreeMap<String, String>,
    pub pages: Vec<Page>,
}

/// Go's `WitnessKey`: conformances instantiated for different generic types.
#[must_use]
pub fn witness_key(descriptor: u64, metadata: u64) -> String {
    format!("{descriptor}:{metadata}")
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_false(value: &bool) -> bool {
    !value
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_zero(value: &i64) -> bool {
    *value == 0
}

fn to_base64<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&STANDARD.encode(data))
}

fn from_base64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(deserializer)?;
    STANDARD.decode(text).map_err(serde::de::Error::custom)
}
