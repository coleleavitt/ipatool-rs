//! Differential check against Go `internal/sap/machimage`. The fixture is recorded by
//! `scripts/machimage-oracle.sh` from the pinned macOS 27.0.1 images; this test replays the
//! same calls and compares every guest memory map and write.
//!
//! Opt-in: `IPATOOL_TEST_SAP_CACHE=1` (needs the SAP asset cache filled).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use ipatool_sap::assets;
use ipatool_sap::machimage::{Image, Memory};
use ipatool_sap::{Error, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct Record {
    prelinked: Vec<Prelinked>,
    images: Vec<ImageRecord>,
}

#[derive(Deserialize)]
struct Prelinked {
    address: u64,
    name: String,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
struct ImageRecord {
    name: String,
    file: String,
    shared: bool,
    base: u64,
    #[serde(default)]
    load_base: u64,
    #[serde(default)]
    kernel_base: u64,
    #[serde(default)]
    prelinked: bool,
    exports: Vec<Export>,
    symbol_count: usize,
    symbols_sha256: String,
    instruction_ranges: Vec<[u64; 2]>,
    resolved: Vec<String>,
    maps: Vec<[u64; 2]>,
    writes: Vec<Write>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Export {
    name: String,
    #[serde(default)]
    address: u64,
    #[serde(default)]
    error: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Write {
    address: u64,
    size: usize,
    sha256: String,
}

#[derive(Default)]
struct Recorder {
    maps: Vec<[u64; 2]>,
    writes: Vec<Write>,
}

impl Memory for Recorder {
    fn mem_map(&mut self, address: u64, size: u64) -> Result<()> {
        self.maps.push([address, size]);
        Ok(())
    }

    fn mem_write(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.writes.push(Write {
            address,
            size: data.len(),
            sha256: hex(&Sha256::digest(data)),
        });
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    })
}

fn resolve_address(name: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash & 0x0000_7fff_ffff_fff0
}

fn file<'b>(bundle: &'b assets::Bundle, name: &str) -> &'b [u8] {
    match name {
        "CoreFP" => &bundle.core_fp,
        "AppleMediaServices" => &bundle.apple_media_services,
        "commerce" => &bundle.commerce,
        "adid" => &bundle.adid,
        "fairplayd" => &bundle.fairplayd,
        "FairPlayIOKit" => &bundle.fairplay_kext,
        _ => panic!("unknown image {name}"),
    }
}

fn replay(expected: &ImageRecord, input: &[u8], prelinked: &[(u64, &str)]) -> Result<ImageRecord> {
    let mut image = if expected.shared {
        Image::open_shared(&expected.name, input)?
    } else {
        Image::open(&expected.name, input)?
    };
    let mut actual = ImageRecord {
        name: expected.name.clone(),
        file: expected.file.clone(),
        shared: expected.shared,
        base: image.base(),
        load_base: expected.load_base,
        kernel_base: expected.kernel_base,
        prelinked: expected.prelinked,
        symbol_count: image.symbols().len(),
        instruction_ranges: image.instruction_ranges(),
        ..ImageRecord::default()
    };

    let mut symbols = Sha256::new();
    for symbol in image.symbols() {
        symbols.update(
            format!(
                "{}\0{}\0{}\0{}\0{}\0{}\n",
                symbol.name,
                symbol.indirect_name,
                symbol.n_type,
                symbol.sect,
                symbol.desc,
                symbol.value
            )
            .as_bytes(),
        );
    }
    actual.symbols_sha256 = hex(&symbols.finalize());

    for export in &expected.exports {
        actual
            .exports
            .push(match image.export(&export.name, expected.load_base) {
                Ok(address) => Export {
                    name: export.name.clone(),
                    address,
                    error: String::new(),
                },
                Err(error) => Export {
                    name: export.name.clone(),
                    address: 0,
                    error: error.to_string(),
                },
            });
    }

    let mut resolved = BTreeSet::new();
    let mut resolve = |name: &str| -> Result<u64> {
        resolved.insert(name.to_owned());
        Ok(resolve_address(name))
    };
    if expected.prelinked {
        image.prepare_prelinked(prelinked, &mut resolve)?;
    } else {
        image.relocate(expected.load_base, &mut resolve)?;
    }
    if expected.kernel_base != 0 {
        image.relocate_kernel_cache_pointers(
            expected.load_base,
            expected.kernel_base,
            &mut resolve,
        )?;
    }
    let mut memory = Recorder::default();
    image.load(&mut memory)?;
    actual.resolved = resolved.into_iter().collect();
    actual.maps = memory.maps;
    actual.writes = memory.writes;
    Ok(actual)
}

#[test]
fn matches_go_machimage() -> Result<()> {
    if std::env::var_os("IPATOOL_TEST_SAP_CACHE").is_none() {
        return Ok(());
    }
    let record: Record = serde_json::from_str(include_str!("fixtures/machimage-oracle.json"))
        .map_err(|error| Error::msg(error.to_string()))?;
    let prelinked: Vec<(u64, &str)> = record
        .prelinked
        .iter()
        .map(|entry| (entry.address, entry.name.as_str()))
        .collect();
    let bundle = assets::load()?;
    for expected in &record.images {
        let actual = replay(expected, file(&bundle, &expected.file), &prelinked)?;
        assert_eq!(actual.base, expected.base, "{} base", expected.name);
        assert_eq!(
            actual.exports, expected.exports,
            "{} exports",
            expected.name
        );
        assert_eq!(
            actual.symbol_count, expected.symbol_count,
            "{} symbols",
            expected.name
        );
        assert_eq!(
            actual.symbols_sha256, expected.symbols_sha256,
            "{} symbols",
            expected.name
        );
        assert_eq!(
            actual.instruction_ranges, expected.instruction_ranges,
            "{} ranges",
            expected.name
        );
        assert_eq!(
            actual.resolved, expected.resolved,
            "{} resolved",
            expected.name
        );
        assert_eq!(actual.maps, expected.maps, "{} maps", expected.name);
        assert_eq!(
            actual.writes.len(),
            expected.writes.len(),
            "{} write count",
            expected.name
        );
        for (index, (actual, wanted)) in actual.writes.iter().zip(&expected.writes).enumerate() {
            assert_eq!(actual, wanted, "{} write {index}", expected.name);
        }
    }
    Ok(())
}
