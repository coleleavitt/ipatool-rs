#[cfg(target_os = "macos")]
use security_framework::passwords::set_generic_password;
use security_framework::passwords::{delete_generic_password, get_generic_password};

use crate::item::StoredItem;
use crate::{Error, Result};

/// errSecItemNotFound
const NOT_FOUND: i32 = -25300;

/// Go's keychain backend (macOS and iOS): a generic password with service = service
/// name and account = key; the password data is the raw item data.
pub(crate) struct MacKeychain {
    service: String,
}

fn map(err: security_framework::base::Error) -> Error {
    if err.code() == NOT_FOUND {
        Error::NotFound
    } else {
        Error::MacOs(err)
    }
}

impl MacKeychain {
    pub fn new(service: String) -> Self {
        Self { service }
    }

    pub fn get(&self, key: &str) -> Result<Vec<u8>> {
        get_generic_password(&self.service, key).map_err(map)
    }

    #[cfg(target_os = "macos")]
    pub fn set(&self, item: &StoredItem) -> Result<()> {
        set_generic_password(&self.service, &item.key, &item.data).map_err(map)
    }

    /// Go's `iosKeyring.Set` (`cmd/keyring_ios.go`): update the item first with a query
    /// of class, service and account only, since `SecItemUpdate` on iOS rejects the
    /// `kSecMatchLimit` and `kSecReturnAttributes` keyring v1.9.0 adds; add it when
    /// it does not exist yet.
    #[cfg(target_os = "ios")]
    pub fn set(&self, item: &StoredItem) -> Result<()> {
        use core_foundation::data::CFData;
        use security_framework::item::{
            ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, ItemUpdateOptions,
            ItemUpdateValue, update_item,
        };

        let mut query = ItemSearchOptions::new();
        query
            .class(ItemClass::generic_password())
            .service(&self.service)
            .account(&item.key);
        let mut attributes = ItemUpdateOptions::new();
        attributes.set_value(ItemUpdateValue::Data(CFData::from_buffer(&item.data)));
        // go-keychain drops empty string attributes.
        if !item.label.is_empty() {
            attributes.set_label(&item.label);
        }
        if !item.description.is_empty() {
            attributes.set_description(&item.description);
        }
        match update_item(&query, &attributes) {
            Ok(()) => return Ok(()),
            Err(err) if err.code() != NOT_FOUND => return Err(Error::IosUpdate(err)),
            Err(_) => {}
        }
        // keyring's own `Set`: add with the label and description.
        let mut add = ItemAddOptions::new(ItemAddValue::Data {
            class: ItemClass::generic_password(),
            data: CFData::from_buffer(&item.data),
        });
        add.set_service(&self.service).set_account_name(&item.key);
        if !item.label.is_empty() {
            add.set_label(&item.label);
        }
        if !item.description.is_empty() {
            add.set_description(&item.description);
        }
        add.add().map_err(map)
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        delete_generic_password(&self.service, key).map_err(map)
    }
}

/// Go's `keyring_ios_test.go`, against the device keychain.
#[cfg(all(test, target_os = "ios"))]
mod tests {
    use super::*;

    #[test]
    fn persists_and_updates_without_invalid_update_parameters() {
        let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        let ring = MacKeychain::new(format!("ipatool-test-{nanos}"));
        ring.set(&StoredItem::new("account", b"first", "ipatool"))
            .unwrap();
        ring.set(&StoredItem::new("account", b"updated", "ipatool"))
            .unwrap();
        let data = ring.get("account");
        ring.remove("account").unwrap();
        assert_eq!(data.unwrap(), b"updated");
    }
}
