//! Port of Go `profilegen/features.go`: the pinned operating system's feature defaults.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::source::Source;

#[derive(Deserialize)]
struct Flag {
    #[serde(rename = "DevelopmentPhase", default)]
    development_phase: String,
}

impl Source<'_> {
    pub(crate) fn features(&self) -> Result<BTreeMap<String, bool>> {
        let mut result = BTreeMap::new();
        for domain in ["AppleMediaServices", "CFNetwork", "Foundation"] {
            let data = self
                .cache
                .feature_flags(domain)
                .map_err(|error| Error::failed(format!("read Store feature defaults: {error}")))?;
            let flags: BTreeMap<String, Flag> = plist::from_bytes(&data)
                .map_err(|error| Error::failed(format!("read Store feature defaults: {error}")))?;
            for (key, flag) in flags {
                if flag.development_phase != "FeatureComplete" {
                    return Err(Error::failed(format!(
                        "unsupported feature phase {} for {domain}/{key}",
                        flag.development_phase
                    )));
                }
                result.insert(format!("{domain}/{key}"), true);
            }
        }
        // Absent flags are disabled, matching the public feature-query ABI.
        for key in [
            "UseNewNetworkStack",
            "AppendWithUnmodifiedParameters",
            "CompressPurchaseRequestBodies",
        ] {
            result
                .entry(format!("AppleMediaServices/{key}"))
                .or_insert(false);
        }
        Ok(result)
    }
}
