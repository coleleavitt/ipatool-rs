use serde::{Deserialize, Serialize};

/// The `account` keychain entry, in the Go JSON layout (`pkg/appstore/account.go`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password_token: String,
    #[serde(
        default,
        rename = "directoryServicesIdentifier",
        skip_serializing_if = "String::is_empty"
    )]
    pub directory_services_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub store_front: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pod: String,
}

/// Keychain keys shared with the Go tool.
pub mod keys {
    pub const ACCOUNT: &str = "account";
    pub const ANISETTE: &str = "anisette-v4";
    pub const KBSYNC: &str = "kbsync-v1";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_go_field_names() {
        let account = Account {
            email: "a@b".into(),
            directory_services_id: "1".into(),
            ..Account::default()
        };
        assert_eq!(
            serde_json::to_string(&account).unwrap(),
            r#"{"email":"a@b","directoryServicesIdentifier":"1"}"#
        );
    }
}
