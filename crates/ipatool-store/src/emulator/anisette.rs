//! Go `anisette.go`: `AppleMediaServices`' Store anisette provider. Type 2 is the Configurator
//! AMD proof, not the `AuthKit` I-MD proof.

use std::collections::BTreeMap;

use super::delegate::install_store_amd_adapter;
use super::runtime::{Runtime, XpcHandler, invoke};
use super::{Context as _, Error, Guest, Payload, Result, Value};
use crate::gourl;

impl Runtime {
    /// Go's `StoreAMDHeaders`: `X-Apple-AMD` and `X-Apple-AMD-M` for `endpoint`, with
    /// `com.apple.adid` XPC messages answered by `xpc`.
    pub fn store_amd_headers(
        &mut self,
        endpoint: &str,
        hardware_id: &[u8],
        xpc: XpcHandler,
    ) -> Result<BTreeMap<String, String>> {
        let parsed = gourl::parse(endpoint)
            .ok()
            .filter(|parsed| parsed.scheme == "https" && !parsed.host.is_empty())
            .ok_or_else(|| Error::msg("invalid Store AMD endpoint"))?;
        if hardware_id.len() != 6 {
            return Err(Error::msg("invalid Store AMD hardware identity"));
        }
        let uc = &mut self.uc;
        uc.st().hardware_id = hardware_id.to_vec();
        uc.st().xpc_sync_handler = Some(xpc);
        install_store_amd_adapter(uc).map_err(|error| error.wrap("guest setup"))?;
        let url = uc
            .new_object("NSURL", Payload::String(parsed.to_string()))
            .context("create Store AMD request")?;
        let class = uc.class_address("NSURLRequest");
        let request =
            invoke(uc, class, "requestWithURL:", &[url]).context("create Store AMD request")?;
        let class = uc.class_address("AMSAnisette");
        let value = invoke(
            uc,
            class,
            "_headersForRequest:account:type:",
            &[request, 0, 2],
        )
        .context("generate Store AMD proof")?;
        let exported = self.export(value).context("export Store AMD proof")?;
        let Value::Dictionary(dictionary) = exported else {
            return Err(Error::msg(format!(
                "AppleMediaServices returned no Store AMD proof ({})",
                exported.go_type()
            )));
        };
        let mut result = BTreeMap::new();
        for key in ["X-Apple-AMD", "X-Apple-AMD-M"] {
            match dictionary.get(key) {
                Some(Value::String(text))
                    if !text.is_empty() && text.len() <= 8192 && !text.contains(['\r', '\n']) =>
                {
                    result.insert(key.to_owned(), text.clone());
                }
                _ => {
                    return Err(Error::msg(
                        "AppleMediaServices returned an incomplete Store AMD proof",
                    ));
                }
            }
        }
        Ok(result)
    }
}
