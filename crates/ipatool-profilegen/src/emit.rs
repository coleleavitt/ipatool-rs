//! Port of Go `profilegen/emit.go`. Go embeds the artifacts in generated Go source; the
//! Rust profile embeds the same three zstd frames (`crates/ipatool-store/profile`), so this
//! emits them as files.

use ipatool_store::profile::Profile;

use crate::error::{Error, Result};
use crate::{FRAMEWORK_UUID, Generated, PLUGIN_UUID};

/// The decoded artifacts `ipatool_store::profile` embeds.
pub struct Artifacts {
    pub metadata: Vec<u8>,
    pub pages: Vec<u8>,
    pub plugin: Vec<u8>,
}

impl Generated {
    /// Go's `GoSource` checks and JSON encoding, without the Go declarations.
    ///
    /// # Errors
    ///
    /// Fails on images other than the pinned ones or an inconsistent page index.
    pub fn artifacts(&self, plugin: Vec<u8>) -> Result<Artifacts> {
        let profile = &self.profile;
        if profile.uuid != FRAMEWORK_UUID || profile.plugin_uuid != PLUGIN_UUID {
            return Err(Error::failed("profile does not match pinned Apple images"));
        }
        if self.pages.is_empty()
            || self.pages.len() % 4096 != 0
            || profile.pages.len() != self.pages.len() / 4096
            || plugin.is_empty()
        {
            return Err(Error::failed("incomplete profile output"));
        }
        for (index, page) in (0u64..).zip(&profile.pages) {
            if page.address % 4096 != 0 || page.offset != index * 4096 {
                return Err(Error::failed("invalid profile page index"));
            }
        }
        Ok(Artifacts {
            metadata: go_json(profile)
                .map_err(|error| Error::failed(format!("encode profile metadata: {error}")))?,
            pages: self.pages.clone(),
            plugin,
        })
    }
}

/// Go's `json.Marshal`: struct field order, sorted map keys and HTML-safe escaping of `<`,
/// `>`, `&`, U+2028 and U+2029, which `serde_json` writes literally. Those characters only
/// occur inside JSON strings, so replacing them in the output is exact.
pub(crate) fn go_json(profile: &Profile) -> serde_json::Result<Vec<u8>> {
    let text = serde_json::to_string(profile)?;
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '<' => escaped.push_str("\\u003c"),
            '>' => escaped.push_str("\\u003e"),
            '&' => escaped.push_str("\\u0026"),
            '\u{2028}' => escaped.push_str("\\u2028"),
            '\u{2029}' => escaped.push_str("\\u2029"),
            _ => escaped.push(character),
        }
    }
    Ok(escaped.into_bytes())
}

/// One zstd frame. Go uses klauspost's best level; `ruzstd` implements only its fastest
/// level, so the frames differ from Go's while the decoded bytes are identical.
#[must_use]
pub fn compress(data: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ipatool_store::profile::Page;

    use super::*;

    fn fixture() -> Generated {
        Generated {
            profile: Profile {
                uuid: FRAMEWORK_UUID.to_owned(),
                plugin_uuid: PLUGIN_UUID.to_owned(),
                pages: vec![Page {
                    address: 0x1_8000_0000,
                    offset: 0,
                }],
                ..Profile::default()
            },
            pages: vec![0; 4096],
        }
    }

    #[test]
    fn is_identical_across_map_insertion_orders_and_round_trips_every_artifact() {
        let (mut first, mut second) = (fixture(), fixture());
        first.profile.symbols = BTreeMap::from([("a".to_owned(), 1), ("z".to_owned(), 2)]);
        second.profile.symbols = BTreeMap::from([("z".to_owned(), 2), ("a".to_owned(), 1)]);
        let plugin = vec![0, 0xff, b'`', b'"', b'\\', b'\n'];
        let a = first.artifacts(plugin.clone()).unwrap();
        let b = second.artifacts(plugin.clone()).unwrap();
        assert_eq!(a.metadata, b.metadata);
        assert_eq!(a.metadata, go_json(&first.profile).unwrap());
        assert_eq!(a.pages, first.pages);
        assert_eq!(a.plugin, plugin);
        for data in [&a.metadata, &a.pages, &a.plugin] {
            let mut decoded = Vec::new();
            std::io::Read::read_to_end(
                &mut ruzstd::decoding::StreamingDecoder::new(compress(data).as_slice()).unwrap(),
                &mut decoded,
            )
            .unwrap();
            assert_eq!(&decoded, data);
        }
    }

    #[test]
    fn rejects_incomplete_output_and_inconsistent_page_offsets() {
        let mut input = fixture();
        assert_eq!(
            input.artifacts(Vec::new()).err().unwrap().to_string(),
            "incomplete profile output"
        );
        input.profile.pages[0].offset = 4096;
        let error = input.artifacts(b"plugin".to_vec()).err().unwrap();
        assert!(error.to_string().contains("page index"), "{error}");
    }

    #[test]
    fn rejects_images_from_a_different_binary_version() {
        let mut input = fixture();
        input.profile.uuid = "different".to_owned();
        let error = input.artifacts(b"plugin".to_vec()).err().unwrap();
        assert!(error.to_string().contains("pinned Apple images"), "{error}");
    }

    #[test]
    fn escapes_like_go_json() {
        let mut profile = Profile::default();
        profile.strings.insert(
            "1".to_owned(),
            "<a & b>\u{2028}\u{2029}\u{1}\"\\é".to_owned(),
        );
        let text = String::from_utf8(go_json(&profile).unwrap()).unwrap();
        assert!(
            text.contains(r#""Strings":{"1":"\u003ca \u0026 b\u003e\u2028\u2029\u0001\"\\é"}"#),
            "{text}"
        );
    }
}
