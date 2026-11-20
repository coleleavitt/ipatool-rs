use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// JSON form of Go's `keyring.Item`, field for field. The Secret Service secret and
/// the decrypted file payload are this JSON. `Data` is a Go `[]byte`: standard base64,
/// `null` when empty.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct StoredItem {
    #[serde(default)]
    pub key: String,
    #[serde(default, serialize_with = "ser_bytes", deserialize_with = "de_bytes")]
    pub data: Vec<u8>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub keychain_not_trust_application: bool,
    #[serde(default)]
    pub keychain_not_synchronizable: bool,
}

impl StoredItem {
    pub fn new(key: &str, data: &[u8], label: &str) -> Self {
        Self {
            key: key.to_owned(),
            data: data.to_vec(),
            label: label.to_owned(),
            description: String::new(),
            keychain_not_trust_application: false,
            keychain_not_synchronizable: false,
        }
    }
}

fn ser_bytes<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    if data.is_empty() {
        serializer.serialize_none()
    } else {
        serializer.serialize_str(&STANDARD.encode(data))
    }
}

fn de_bytes<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let encoded = Option::<String>::deserialize(deserializer)?.unwrap_or_default();
    STANDARD.decode(encoded).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_go_json() {
        let item = StoredItem::new("account", b"{}", "ipatool-auth.service");
        assert_eq!(
            serde_json::to_string(&item).unwrap(),
            r#"{"Key":"account","Data":"e30=","Label":"ipatool-auth.service","Description":"","KeychainNotTrustApplication":false,"KeychainNotSynchronizable":false}"#
        );
    }

    #[test]
    fn reads_null_data() {
        let item: StoredItem = serde_json::from_str(r#"{"Key":"k","Data":null}"#).unwrap();
        assert_eq!(item.data, Vec::<u8>::new());
    }
}
