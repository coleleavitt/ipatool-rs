use std::fmt;
use std::str::FromStr;

use serde::Serialize;

use crate::Error;

/// A store platform. `Unknown` only appears in output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Unknown,
    #[serde(rename = "iphone")]
    IPhone,
    #[serde(rename = "ipad")]
    IPad,
    #[serde(rename = "appletv")]
    AppleTv,
    #[serde(rename = "watchos")]
    WatchOs,
    #[serde(rename = "visionos")]
    VisionOs,
    #[serde(rename = "macos")]
    MacOs,
}

impl Platform {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::IPhone => "iphone",
            Self::IPad => "ipad",
            Self::AppleTv => "appletv",
            Self::WatchOs => "watchos",
            Self::VisionOs => "visionos",
            Self::MacOs => "macos",
        }
    }

    /// The iTunes `entity` for a lookup. `None` means no platform was chosen.
    pub(crate) fn lookup_entity(platform: Option<Self>) -> Result<&'static str, Error> {
        Ok(match platform {
            None => "software,iPadSoftware",
            Some(Self::IPhone) => "software",
            Some(Self::IPad) => "iPadSoftware",
            Some(Self::AppleTv) => "tvSoftware",
            Some(Self::WatchOs) => "watchSoftware",
            Some(Self::VisionOs) => "xrosSoftware",
            Some(Self::MacOs) => "macSoftware",
            Some(Self::Unknown) => return Err(Error::InvalidPlatform("unknown".into())),
        })
    }

    /// The iTunes `entity` for a search. Watch and TV apps share the iOS catalog.
    pub(crate) fn search_entity(platform: Option<Self>) -> Result<&'static str, Error> {
        Ok(match platform {
            Some(Self::AppleTv) => "software,tvSoftware",
            Some(Self::WatchOs) => "software,watchSoftware",
            other => Self::lookup_entity(other)?,
        })
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Platform {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Error> {
        Ok(match value.to_ascii_lowercase().as_str() {
            "iphone" | "ios" => Self::IPhone,
            "ipad" | "ipados" => Self::IPad,
            "appletv" | "apple-tv" | "tvos" => Self::AppleTv,
            "watch" | "watchos" | "applewatch" | "apple-watch" => Self::WatchOs,
            "vision" | "visionos" | "visionpro" | "xros" | "realitydevice" => Self::VisionOs,
            "mac" | "macos" | "osx" => Self::MacOs,
            _ => return Err(Error::InvalidPlatform(value.to_owned())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases() {
        assert_eq!("iOS".parse::<Platform>().unwrap(), Platform::IPhone);
        assert_eq!("xros".parse::<Platform>().unwrap(), Platform::VisionOs);
        assert!("android".parse::<Platform>().is_err());
    }

    #[test]
    fn search_entities_match_go() {
        assert_eq!(
            Platform::search_entity(None).unwrap(),
            "software,iPadSoftware"
        );
        assert_eq!(
            Platform::search_entity(Some(Platform::WatchOs)).unwrap(),
            "software,watchSoftware"
        );
        assert_eq!(
            Platform::lookup_entity(Some(Platform::WatchOs)).unwrap(),
            "watchSoftware"
        );
    }
}
