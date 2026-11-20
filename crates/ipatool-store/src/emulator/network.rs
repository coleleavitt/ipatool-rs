//! Placeholder for Go `network.go`, ported on the `phase5-network` branch, which replaces
//! this file. It holds only the request constructor the Store AMD flow sends.

use std::collections::{BTreeMap, HashMap};

use super::{Guest, Payload, Result, Uc};

#[derive(Default)]
pub(crate) struct NetworkState {}

pub(crate) fn network_message(
    uc: &mut Uc,
    _receiver: u64,
    class: &str,
    selector: &str,
    meta: bool,
) -> Result<bool> {
    if meta
        && (class == "NSURLRequest" || class == "NSMutableURLRequest")
        && selector == "requestWithURL:"
    {
        let request = uc.new_object(class, Payload::Nil)?;
        let method = uc.new_object("NSString", Payload::String("GET".to_owned()))?;
        let fields = uc.new_object("NSMutableDictionary", Payload::Dict(BTreeMap::new()))?;
        let url = uc.arg(2)?;
        uc.obj_mut(request)?.properties = HashMap::from([
            ("URL".to_owned(), url),
            ("HTTPMethod".to_owned(), method),
            ("allHTTPHeaderFields".to_owned(), fields),
        ]);
        uc.set(request)?;
        return Ok(true);
    }
    Ok(false)
}
