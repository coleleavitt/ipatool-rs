use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

use crate::item::StoredItem;
use crate::{Error, Result};

/// errSecItemNotFound
const NOT_FOUND: i32 = -25300;

/// Go's keychain backend: a generic password with service = service name and
/// account = key; the password data is the raw item data.
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

    pub fn set(&self, item: &StoredItem) -> Result<()> {
        set_generic_password(&self.service, &item.key, &item.data).map_err(map)
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        delete_generic_password(&self.service, key).map_err(map)
    }
}
