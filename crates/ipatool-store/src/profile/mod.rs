//! Go `internal/store/profile`: the precomputed Store profile. The three artifacts are the
//! zstd frames Go embeds in `data_generated.go`, extracted by
//! `scripts/extract-store-profile.py`, so both tools decode identical bytes.

mod format;

use std::io::Read as _;
use std::sync::OnceLock;

use sha2::{Digest as _, Sha256};

pub use format::{
    Call, Class, Constant, DictionaryLayout, MetadataPatch, Method, Page, Profile, witness_key,
};

/// Go's `generatedFrameworkUUID` and `generatedPluginUUID`.
pub const FRAMEWORK_UUID: &str = "5DFC37A5-4689-3F9D-9655-D4774E1D8D08";
pub const PLUGIN_UUID: &str = "B8C07F94-4868-3CD7-A59A-F01E8C24C1B7";
/// The profile generator inputs Go records in `data_generated.go`.
pub const PINNED_INPUTS: &str = "macos-27-26A434-unicorn-3-11d75d28e8c328de";

const PAGE_SIZE: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid compiled {0} size")]
    Size(&'static str),
    #[error("decode compiled {name}: {source}")]
    Decode {
        name: &'static str,
        source: std::io::Error,
    },
    #[error("compiled {0} has an invalid decoded size")]
    DecodedSize(&'static str),
    #[error("compiled {0} failed integrity verification")]
    Integrity(&'static str),
    #[error("decode Store metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("compiled Store profile uses unexpected Apple images")]
    Images,
    #[error("incomplete compiled Store profile")]
    Incomplete,
    #[error("invalid Store page index")]
    PageIndex,
}

struct Artifact {
    name: &'static str,
    size: usize,
    sha256: &'static str,
    limit: usize,
    compressed: &'static [u8],
}

const METADATA: Artifact = Artifact {
    name: "Store metadata",
    size: 4_807_917,
    sha256: "5e92717743272aa84343bd25f6bab6401155f88ade51b704ec535d3e6b5bd421",
    limit: 128 << 20,
    compressed: include_bytes!("../../profile/metadata.json.zst"),
};

const PAGES: Artifact = Artifact {
    name: "Store pages",
    size: 20_893_696,
    sha256: "4fd8cf193ca6e71d52a2d6e68d9ca76907332ff33dade332351c97efe5f0caed",
    limit: 256 << 20,
    compressed: include_bytes!("../../profile/pages.bin.zst"),
};

const PLUGIN: Artifact = Artifact {
    name: "Store authentication plugin",
    size: 138_088,
    sha256: "e33fff05a6e6593dd91e33fd103b5d3068b62a787c680df33b859f19b3acc4ea",
    limit: 64 << 20,
    compressed: include_bytes!("../../profile/plugin.zst"),
};

/// Go's `profile.Data`: the metadata JSON, the initialized guest pages and the plugin image,
/// with the metadata also decoded.
#[derive(Debug)]
pub struct Data {
    pub metadata: Vec<u8>,
    pub profile: Profile,
    pub pages: Vec<u8>,
    pub plugin: Vec<u8>,
}

/// Go's `profile.Load`: decodes the embedded profile once per process.
///
/// # Errors
/// An artifact fails its size or digest check, or the metadata does not match the pages.
pub fn load() -> Result<&'static Data, &'static Error> {
    static DATA: OnceLock<Result<Data, Error>> = OnceLock::new();
    DATA.get_or_init(decode).as_ref()
}

fn decode() -> Result<Data, Error> {
    let metadata = decode_artifact(&METADATA)?;
    let pages = decode_artifact(&PAGES)?;
    let plugin = decode_artifact(&PLUGIN)?;
    let profile: Profile = serde_json::from_slice(&metadata)?;
    validate(&profile, &pages, &plugin)?;
    Ok(Data {
        metadata,
        profile,
        pages,
        plugin,
    })
}

/// Go's `decodeArtifact`: bounded decompression plus the raw digest.
fn decode_artifact(item: &Artifact) -> Result<Vec<u8>, Error> {
    if item.size == 0 || item.size > item.limit {
        return Err(Error::Size(item.name));
    }
    let decoder = ruzstd::decoding::StreamingDecoder::new(item.compressed).map_err(|error| {
        Error::Decode {
            name: item.name,
            source: std::io::Error::other(error.to_string()),
        }
    })?;
    let mut data = Vec::with_capacity(item.size);
    decoder
        .take(item.size as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|source| Error::Decode {
            name: item.name,
            source,
        })?;
    if data.len() != item.size {
        return Err(Error::DecodedSize(item.name));
    }
    if hex(&Sha256::digest(&data)) != item.sha256 {
        return Err(Error::Integrity(item.name));
    }
    Ok(data)
}

/// Go's `validateData`.
fn validate(profile: &Profile, pages: &[u8], plugin: &[u8]) -> Result<(), Error> {
    if profile.uuid != FRAMEWORK_UUID || profile.plugin_uuid != PLUGIN_UUID {
        return Err(Error::Images);
    }
    if profile.pages.is_empty()
        || profile.pages.len() != pages.len() / PAGE_SIZE
        || !pages.len().is_multiple_of(PAGE_SIZE)
        || plugin.is_empty()
    {
        return Err(Error::Incomplete);
    }
    for (index, page) in profile.pages.iter().enumerate() {
        if page.address % PAGE_SIZE as u64 != 0 || page.offset != (index * PAGE_SIZE) as u64 {
            return Err(Error::PageIndex);
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[usize::from(byte >> 4)],
                DIGITS[usize::from(byte & 15)],
            ]
        })
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_embedded_profile() {
        let data = load().unwrap();
        assert_eq!(data.pages.len(), PAGES.size);
        assert_eq!(data.plugin.len(), PLUGIN.size);
        assert_eq!(data.profile.uuid, FRAMEWORK_UUID);
        assert_eq!(data.profile.objc_metadata["NSString"], 8_158_823_584);
        assert!(!data.profile.classes.is_empty());
        assert!(!data.profile.calls.is_empty());
        // The decoded struct re-encodes to the same JSON values Go wrote.
        let original: serde_json::Value = serde_json::from_slice(&data.metadata).unwrap();
        assert_eq!(serde_json::to_value(&data.profile).unwrap(), original);
    }

    #[test]
    fn rejects_a_corrupt_artifact() {
        let item = Artifact {
            sha256: "00",
            ..PLUGIN
        };
        assert_eq!(
            decode_artifact(&item).unwrap_err().to_string(),
            "compiled Store authentication plugin failed integrity verification"
        );
        let item = Artifact {
            size: PLUGIN.size - 1,
            ..PLUGIN
        };
        assert_eq!(
            decode_artifact(&item).unwrap_err().to_string(),
            "compiled Store authentication plugin has an invalid decoded size"
        );
    }

    #[test]
    fn validates_pages_like_go() {
        let data = load().unwrap();
        let mut profile = data.profile.clone();
        profile.pages[1].offset = 0;
        assert!(matches!(
            validate(&profile, &data.pages, &data.plugin),
            Err(Error::PageIndex)
        ));
        profile.uuid = String::new();
        assert!(matches!(
            validate(&profile, &data.pages, &data.plugin),
            Err(Error::Images)
        ));
    }
}
