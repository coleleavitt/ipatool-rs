//! Go `bag.go`: a frozen `AMSPurchase` bag built from host values.

use std::collections::BTreeMap;

use super::bridge::import_value;
use super::runtime::invoke;
use super::value::Date;
use super::{Guest, Payload, Result, Uc, Value};

pub(crate) fn frozen_bag_from_values(uc: &mut Uc, values: &BTreeMap<String, Value>) -> Result<u64> {
    let builder = uc.new_object("AMSBagFrozenDataSourceBuilder", Payload::Nil)?;
    let data = import_value(uc, &Value::Dictionary(values.clone()))?;
    let profile = uc.new_object("NSString", Payload::String("AMSPurchase".to_owned()))?;
    let version = uc.new_object("NSString", Payload::String("1".to_owned()))?;
    let expiration = uc.new_object("NSDate", Payload::Date(Date::now().add_seconds(5 * 60)))?;
    for (selector, value) in [
        ("setData:", data),
        ("setProfile:", profile),
        ("setProfileVersion:", version),
        ("setExpirationDate:", expiration),
    ] {
        invoke(uc, builder, selector, &[value])?;
    }
    invoke(uc, builder, "createFrozenBag", &[])
}
