//! Go `delegate.go`: the declining session delegate and the Store AMD XPC adapter.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use super::bridge::cocoa_error;
use super::callbacks::callback;
use super::runtime::{Callback, HostFn, RET, host_block};
use super::value::Date;
use super::{Guest, Payload, Result, Uc, fail, method_key};
use crate::profile::Class;

/// The CLI owns interactive authentication. This delegate declines native UI so Apple's
/// session can finish with an error and the CLI can interpret the server's 2FA,
/// expired-token, or license response.
#[allow(dead_code, reason = "client.rs uses it")]
pub(crate) fn interactive_delegate(uc: &mut Uc) -> Result<u64> {
    const NAME: &str = "IPAToolStoreDelegate";
    if !uc.sti().classes.contains_key(NAME) {
        let address = uc.alloc(16)?;
        uc.st().classes.insert(
            NAME.to_owned(),
            Arc::new(Class {
                name: NAME.to_owned(),
                address,
                superclass: "NSObject".to_owned(),
                size: 16,
                ivars: Some(BTreeMap::new()),
                ..Class::default()
            }),
        );
        uc.st().class_names.insert(address, NAME.to_owned());
        for (code, selector) in [
            (
                1,
                "AMSURLSession:task:handleAuthenticateRequest:completion:",
            ),
            (2, "AMSURLSession:task:handleDialogRequest:completion:"),
        ] {
            let block = host_block(
                uc,
                Rc::new(move |uc: &mut Uc| {
                    let completion = uc.arg(5)?;
                    let failure =
                        cocoa_error(uc, "IPAToolStoreInteraction", code, BTreeMap::new())?;
                    callback(uc, completion, &[0, failure], Box::new(|uc| uc.set(0)))
                }),
            )?;
            let entry = uc.read64(block + 16)?;
            uc.st()
                .methods
                .insert(method_key(NAME, selector, false), entry);
        }
    }
    uc.new_object(NAME, Payload::Nil)
}

fn install(uc: &mut Uc, address: u64, callback: HostFn) -> Result<()> {
    let page = address & !4095;
    if !uc.sti().pages.contains(&page) {
        uc.map_mem(page, 4096)?;
    }
    uc.write_mem(address, &RET)?;
    uc.st()
        .callbacks
        .insert(address, Callback::Shared(callback));
    Ok(())
}

fn zero(uc: &mut Uc) -> Result<()> {
    uc.set(0)
}

/// Go's `installStoreAMDAdapter`: libxpc and notify entry points the anisette provider calls,
/// with `com.apple.adid` messages answered by the XPC handler.
pub(crate) fn install_store_amd_adapter(uc: &mut Uc) -> Result<()> {
    install(
        uc,
        0x1_8057_4a18,
        Rc::new(|uc: &mut Uc| {
            let dictionary = uc.new_object("HostXPCDictionary", Payload::Dict(BTreeMap::new()))?;
            uc.set(dictionary)
        }),
    )?;
    install(
        uc,
        0x1_8055_3470,
        Rc::new(|uc: &mut Uc| {
            let dictionary = uc.arg(0)?;
            uc.dict(dictionary)?;
            let length = uc.arg(3)?;
            if length > 1 << 20 {
                fail!("XPC data exceeds runtime limit");
            }
            let data = uc.read_mem(uc.arg(2)?, length)?;
            let key = uc.cstring(uc.arg(1)?)?;
            let value = uc.new_object("NSData", Payload::Data(data))?;
            uc.dict_mut(dictionary)?.insert(key, value);
            uc.set(0)
        }),
    )?;
    install(
        uc,
        0x1_8055_3a00,
        Rc::new(|uc: &mut Uc| {
            let key = uc.cstring(uc.arg(1)?)?;
            let item = uc.dict(uc.arg(0)?)?.get(&key).copied().unwrap_or_default();
            let out = uc.arg(2)?;
            if !uc.sti().objects.contains_key(&item) {
                if out != 0 {
                    uc.write64(out, 0)?;
                }
                return uc.set(0);
            }
            let data = uc.data(item)?.clone();
            let pointer = uc.alloc(data.len().max(1) as u64)?;
            if !data.is_empty() {
                uc.write_mem(pointer, &data)?;
            }
            if out != 0 {
                uc.write64(out, data.len() as u64)?;
            }
            uc.set(pointer)
        }),
    )?;
    install(uc, 0x1_8054_c6b8, Rc::new(zero))?;
    install(
        uc,
        0x1_8055_4c88,
        Rc::new(|uc: &mut Uc| {
            let name = uc.cstring(uc.arg(0)?)?;
            let connection = uc.new_object("HostXPCConnection", Payload::String(name))?;
            let (queue, flags) = (uc.arg(1)?, uc.arg(2)?);
            let object = uc.obj_mut(connection)?;
            object.properties.insert("targetQueue".to_owned(), queue);
            object.properties.insert("flags".to_owned(), flags);
            uc.set(connection)
        }),
    )?;
    install(
        uc,
        0x1_8055_4dac,
        Rc::new(|uc: &mut Uc| {
            let handler = uc.arg(1)?;
            uc.obj_mut(uc.arg(0)?)?
                .properties
                .insert("eventHandler".to_owned(), handler);
            uc.set(0)
        }),
    )?;
    install(uc, 0x1_8055_50b4, Rc::new(zero))?;
    install(
        uc,
        0x1_8055_6440,
        Rc::new(|uc: &mut Uc| {
            let service = uc.obj(uc.arg(0)?)?.value.sprint();
            let data = uc
                .dict(uc.arg(1)?)?
                .get("data")
                .copied()
                .unwrap_or_default();
            let request = match uc.sti().objects.get(&data) {
                Some(_) if uc.sti().xpc_sync_handler.is_some() => uc.data(data)?.clone(),
                _ => fail!("Store AMD XPC handler is unavailable"),
            };
            let mut handler = uc.st().xpc_sync_handler.take();
            let reply = handler.as_mut().map(|handler| handler(&service, &request));
            uc.st().xpc_sync_handler = handler;
            let Some(reply) = reply else {
                fail!("Store AMD XPC handler is unavailable");
            };
            let reply = uc.new_object("NSData", Payload::Data(reply?))?;
            let dictionary = uc.new_object(
                "HostXPCDictionary",
                Payload::Dict(BTreeMap::from([("data".to_owned(), reply)])),
            )?;
            uc.set(dictionary)
        }),
    )?;
    install(
        uc,
        0x1_84e5_0b38,
        Rc::new(|uc: &mut Uc| {
            let token = uc.arg(1)?;
            if token != 0 {
                uc.write64(token, 1)?;
            }
            uc.set(0)
        }),
    )?;
    install(
        uc,
        0x1_84e5_1370,
        Rc::new(|uc: &mut Uc| {
            let changed = uc.arg(1)?;
            if changed != 0 {
                uc.write64(changed, 0)?;
            }
            uc.set(0)
        }),
    )?;
    install(uc, 0x1_8073_bc1c, Rc::new(zero))?;
    install(
        uc,
        0x1_8071_bec4,
        Rc::new(|uc: &mut Uc| {
            let now = Date::now();
            let value = uc.arg(0)?;
            if value != 0 {
                #[allow(clippy::cast_sign_loss, reason = "Go's uint64(Unix())")]
                uc.write64(value, now.seconds as u64)?;
                uc.write64(value + 8, u64::from(now.nanos / 1000))?;
            }
            uc.set(0)
        }),
    )
}
