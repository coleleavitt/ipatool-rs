//! Go `notifications.go`: a process-local `NSNotificationCenter`.

use std::collections::HashMap;
use std::rc::Rc;

use super::callbacks::delegate_call;
use super::runtime::host_block;
use super::{Guest, Object, Payload, Result, Uc};

pub(crate) fn notification_message(
    uc: &mut Uc,
    receiver: u64,
    class: &str,
    selector: &str,
    meta: bool,
) -> Result<bool> {
    if meta && class == "NSNotificationCenter" && selector == "defaultCenter" {
        if uc.sti().notification_center == 0 {
            let center = uc.new_object(class, Payload::Nil)?;
            uc.st().notification_center = center;
        }
        let center = uc.sti().notification_center;
        uc.set(center)?;
        return Ok(true);
    }
    if class == "NSNotification" {
        if matches!(selector, "name" | "object" | "userInfo") {
            let value = uc.obj(receiver)?.prop(selector);
            uc.set(value)?;
            return Ok(true);
        }
        return Ok(false);
    }
    if meta || class != "NSNotificationCenter" {
        return Ok(false);
    }
    match selector {
        "addObserver:selector:name:object:" => {
            let observer = uc.new_object("HostObserver", Payload::Nil)?;
            let properties = HashMap::from([
                ("observer".to_owned(), uc.arg(2)?),
                ("selector".to_owned(), uc.arg(3)?),
                ("name".to_owned(), uc.arg(4)?),
                ("object".to_owned(), uc.arg(5)?),
            ]);
            uc.obj_mut(observer)?.properties = properties;
            uc.st().observers.insert(observer);
            uc.set(0)?;
        }
        "postNotificationName:object:userInfo:" | "postNotificationName:object:" => {
            let (name, object) = (uc.arg(2)?, uc.arg(3)?);
            let info = if selector == "postNotificationName:object:userInfo:" {
                uc.arg(4)?
            } else {
                0
            };
            let notification = uc.new_object("NSNotification", Payload::Nil)?;
            uc.obj_mut(notification)?.properties = HashMap::from([
                ("name".to_owned(), name),
                ("object".to_owned(), object),
                ("userInfo".to_owned(), info),
            ]);
            let observers: Vec<Object> = uc
                .sti()
                .observers
                .iter()
                .filter_map(|address| uc.sti().objects.get(address).cloned())
                .collect();
            for observer in observers {
                let wanted = observer.prop("name");
                if wanted != 0 && uc.string(wanted)? != uc.string(name)? {
                    continue;
                }
                let source = observer.prop("object");
                if source != 0 && source != object {
                    continue;
                }
                let target = observer.prop("observer");
                let selector = uc.cstring(observer.prop("selector"))?;
                let block = host_block(
                    uc,
                    Rc::new(move |uc: &mut Uc| {
                        delegate_call(
                            uc,
                            target,
                            &selector,
                            vec![notification],
                            Box::new(|uc| uc.set(0)),
                        )
                    }),
                )?;
                uc.st().pending.push(block);
            }
            uc.set(0)?;
        }
        "removeObserver:" | "removeObserver:name:object:" => {
            let target = uc.arg(2)?;
            let state = uc.st();
            let removed: Vec<u64> = state
                .observers
                .iter()
                .copied()
                .filter(|&key| {
                    key == target
                        || state
                            .objects
                            .get(&key)
                            .is_some_and(|object| object.prop("observer") == target)
                })
                .collect();
            for key in removed {
                state.observers.remove(&key);
            }
            uc.set(0)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}
