//! Go `accounts.go`: ephemeral Accounts-framework values, with no host account store or
//! keychain writes.

use std::collections::BTreeMap;

use super::bridge::new_uuid_string;
use super::messages::setter_key;
use super::{Guest, Payload, Result, Uc};

pub(crate) fn account_message(
    uc: &mut Uc,
    receiver: u64,
    class: &str,
    selector: &str,
    meta: bool,
) -> Result<bool> {
    if !meta && (class == "ACAccountStore" || class == "ACDAccountStore") {
        match selector {
            "accountTypeWithAccountTypeIdentifier:"
            | "accountTypeWithAccountTypeIdentifier:error:" => {
                let account_type = uc.new_object("ACAccountType", Payload::Nil)?;
                let identifier = uc.arg(2)?;
                uc.obj_mut(account_type)?.properties =
                    [("identifier".to_owned(), identifier)].into();
                if selector == "accountTypeWithAccountTypeIdentifier:error:" && uc.arg(3)? != 0 {
                    uc.write64(uc.arg(3)?, 0)?;
                }
                uc.set(account_type)?;
                return Ok(true);
            }
            "accountsWithAccountType:"
            | "accountsWithAccountType:options:error:"
            | "accountsWithAccountType:options:classification:error:" => {
                if selector == "accountsWithAccountType:options:error:" && uc.arg(4)? != 0 {
                    uc.write64(uc.arg(4)?, 0)?;
                }
                if selector == "accountsWithAccountType:options:classification:error:"
                    && uc.arg(5)? != 0
                {
                    uc.write64(uc.arg(5)?, 0)?;
                }
                let accounts = uc.new_object("NSArray", Payload::Array(Vec::new()))?;
                uc.set(accounts)?;
                return Ok(true);
            }
            "accounts" => {
                let accounts = uc.new_object("NSArray", Payload::Array(Vec::new()))?;
                uc.set(accounts)?;
                return Ok(true);
            }
            "accountWithIdentifier:" => {
                uc.set(0)?;
                return Ok(true);
            }
            _ => {}
        }
    }
    if meta
        || (!uc.kind_of(class, "ACAccount")
            && class != "ACAccountType"
            && class != "ACAccountCredential")
    {
        return Ok(false);
    }
    let insert = |uc: &mut Uc, key: &str, value: u64| -> Result<()> {
        uc.obj_mut(receiver)?
            .properties
            .insert(key.to_owned(), value);
        Ok(())
    };
    match selector {
        "initWithIdentifier:description:" => {
            insert(uc, "identifier", uc.arg(2)?)?;
            insert(uc, "accountTypeDescription", uc.arg(3)?)?;
            uc.set(receiver)?;
        }
        "initWithAccountType:" => {
            let identifier = uc.new_object("NSString", Payload::String(new_uuid_string()?))?;
            insert(uc, "identifier", identifier)?;
            insert(uc, "accountType", uc.arg(2)?)?;
            let properties =
                uc.new_object("NSMutableDictionary", Payload::Dict(BTreeMap::new()))?;
            insert(uc, "accountProperties", properties)?;
            uc.set(receiver)?;
        }
        "initWithPassword:" => {
            insert(uc, "password", uc.arg(2)?)?;
            uc.set(receiver)?;
        }
        "accountPropertyForKey:" => {
            let fields = uc.obj(receiver)?.prop("accountProperties");
            let key = uc.string(uc.arg(2)?)?;
            let value = uc.dict(fields)?.get(&key).copied().unwrap_or_default();
            uc.set(value)?;
        }
        "setAccountProperty:forKey:" => {
            let fields = uc.obj(receiver)?.prop("accountProperties");
            uc.dict(fields)?;
            let key = uc.string(uc.arg(3)?)?;
            let value = uc.arg(2)?;
            let fields = uc.dict_mut(fields)?;
            if value == 0 {
                fields.remove(&key);
            } else {
                fields.insert(key, value);
            }
            uc.set(0)?;
        }
        "accountType" | "identifier" | "username" | "credential" | "accountProperties"
        | "authenticated" | "active" | "owningBundleID" | "accountDescription"
        | "parentAccount" | "isDirty" => {
            let value = uc.obj(receiver)?.prop(selector);
            uc.set(value)?;
        }
        _ => {
            let Some(key) = setter_key(selector) else {
                return Ok(false);
            };
            insert(uc, &key, uc.arg(2)?)?;
            uc.set(0)?;
        }
    }
    Ok(true)
}
