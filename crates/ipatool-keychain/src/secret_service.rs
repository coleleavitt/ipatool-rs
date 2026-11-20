use std::collections::HashMap;

use secret_service::{Collection, EncryptionType, SecretService};

use crate::item::StoredItem;
use crate::{Error, Result};

/// Go's `secretsKeyring`: a collection whose D-Bus path is
/// `/org/freedesktop/secrets/collection/<service name>` (after decoding `_XX` escapes),
/// with one item per key, labelled with the key and found by `profile=<key>`.
pub(crate) struct SecretServiceKeyring {
    service: SecretService<'static>,
    name: String,
}

const COLLECTION_PREFIX: &str = "/org/freedesktop/secrets/collection/";
const PROFILE: &str = "profile";

impl SecretServiceKeyring {
    /// `None` when no session bus or Secret Service answers, so the caller falls back.
    pub async fn open(name: &str) -> Option<Self> {
        let connection = session_bus().await?;
        let service = SecretService::connect_with_existing(EncryptionType::Dh, connection)
            .await
            .ok()?;
        Some(Self {
            service,
            name: name.to_owned(),
        })
    }

    async fn collection(&self) -> Result<Option<Collection<'_>>> {
        let wanted = format!("{COLLECTION_PREFIX}{}", self.name);
        for collection in self.service.get_all_collections().await? {
            if decode_path(collection.collection_path.as_str()) == wanted {
                return Ok(Some(collection));
            }
        }
        Ok(None)
    }

    pub async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let Some(collection) = self.collection().await? else {
            return Err(Error::NotFound);
        };
        let items = collection
            .search_items(HashMap::from([(PROFILE, key)]))
            .await?;
        let item = items.first().ok_or(Error::NotFound)?;
        if item.is_locked().await? {
            item.unlock().await?;
        }
        let secret = item.get_secret().await?;
        Ok(serde_json::from_slice::<StoredItem>(&secret)?.data)
    }

    pub async fn set(&self, item: &StoredItem) -> Result<()> {
        let collection = match self.collection().await? {
            Some(collection) => collection,
            None => self.service.create_collection(&self.name, "").await?,
        };
        if collection.is_locked().await? {
            collection.unlock().await?;
        }
        collection
            .create_item(
                &item.key,
                HashMap::from([(PROFILE, item.key.as_str())]),
                &serde_json::to_vec(item)?,
                true,
                "application/json",
            )
            .await?;
        Ok(())
    }

    pub async fn remove(&self, key: &str) -> Result<()> {
        let Some(collection) = self.collection().await? else {
            return Err(Error::NotFound);
        };
        let items = collection
            .search_items(HashMap::from([(PROFILE, key)]))
            .await?;
        if let Some(item) = items.first() {
            if item.is_locked().await? {
                item.unlock().await?;
            }
            item.delete().await?;
        }
        Ok(())
    }
}

/// Connects like Go's godbus: tries each `;`-separated session bus address in order
/// and ignores `guid=`. zbus alone checks the GUID and uses one address, so a
/// `DBUS_SESSION_BUS_ADDRESS` left over from before a bus restart breaks it.
async fn session_bus() -> Option<zbus::Connection> {
    let addresses = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_else(|_| {
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        format!("unix:path={runtime}/bus")
    });
    for address in addresses.split(';').map(without_guid) {
        let Ok(builder) = zbus::connection::Builder::address(address.as_str()) else {
            continue;
        };
        if let Ok(connection) = builder.build().await {
            return Some(connection);
        }
    }
    None
}

fn without_guid(address: &str) -> String {
    let Some((transport, params)) = address.split_once(':') else {
        return address.to_owned();
    };
    let params: Vec<&str> = params
        .split(',')
        .filter(|param| !param.starts_with("guid="))
        .collect();
    format!("{transport}:{}", params.join(","))
}

/// Go's `decodeKeyringString`: D-Bus object paths escape bytes as `_XX`. Returns the
/// input unchanged when an escape is malformed.
fn decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' {
            let Some(byte) = bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            else {
                return path.to_owned();
            };
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{decode_path, without_guid};

    #[test]
    fn drops_guid() {
        assert_eq!(
            without_guid("unix:path=/run/user/1000/bus,guid=651f"),
            "unix:path=/run/user/1000/bus"
        );
    }

    #[test]
    fn decodes_dbus_escapes() {
        assert_eq!(
            decode_path("/org/freedesktop/secrets/collection/ipatool_2dauth_2eservice"),
            "/org/freedesktop/secrets/collection/ipatool-auth.service"
        );
        assert_eq!(decode_path("/a/b_zz"), "/a/b_zz");
    }
}
