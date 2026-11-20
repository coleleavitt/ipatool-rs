use serde::{Deserialize, Serialize};

use crate::Platform;

/// An app as returned by the iTunes search and lookup APIs.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct App {
    pub id: i64,
    #[serde(rename = "bundleID")]
    pub bundle_id: String,
    pub name: String,
    pub version: String,
    #[serde(serialize_with = "go_float")]
    pub price: f64,
    pub platforms: Vec<Platform>,
}

/// Go's zerolog writes whole floats without a fraction (`0`, not `0.0`).
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde serialize_with signature"
)]
fn go_float<S: serde::Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "only whole values in i64 range"
    )]
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ItunesApp {
    track_id: i64,
    #[serde(default)]
    bundle_id: String,
    #[serde(default)]
    track_name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    price: f64,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    supported_devices: Vec<String>,
}

const DEVICE_FAMILIES: [(&str, Platform); 7] = [
    ("iPhone", Platform::IPhone),
    ("iPod", Platform::IPhone),
    ("iPad", Platform::IPad),
    ("AppleTV", Platform::AppleTv),
    ("Watch", Platform::WatchOs),
    ("RealityDevice", Platform::VisionOs),
    ("Mac", Platform::MacOs),
];

impl From<ItunesApp> for App {
    fn from(item: ItunesApp) -> Self {
        let mut platforms = Vec::new();
        for (prefix, platform) in DEVICE_FAMILIES {
            if !platforms.contains(&platform)
                && item
                    .supported_devices
                    .iter()
                    .any(|device| device.starts_with(prefix))
            {
                platforms.push(platform);
            }
        }
        if item.kind == "mac-software" && !platforms.contains(&Platform::MacOs) {
            platforms.push(Platform::MacOs);
        }
        if platforms.is_empty() {
            platforms.push(Platform::Unknown);
        }
        Self {
            id: item.track_id,
            bundle_id: item.bundle_id,
            name: item.track_name,
            version: item.version,
            price: item.price,
            platforms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_serializes_like_go() {
        let mut app = App {
            id: 1,
            bundle_id: String::new(),
            name: String::new(),
            version: String::new(),
            price: 0.0,
            platforms: vec![],
        };
        assert!(
            serde_json::to_string(&app)
                .unwrap()
                .contains(r#""price":0,"#)
        );
        app.price = 2.99;
        assert!(
            serde_json::to_string(&app)
                .unwrap()
                .contains(r#""price":2.99,"#)
        );
    }
}
