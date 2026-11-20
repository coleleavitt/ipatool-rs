//! Port of Go `assets/kernel.go`: the `FairPlayIOKit` kext from the pinned kernel collection.

use std::collections::HashMap;
use std::io::Read;

use super::macho::{self, N_ABS, N_EXT, Symbol};
use super::{Archive, pinned_zip_entry};
use crate::error::{Context, Error, Result};

const KERNEL_CACHE_NAME: &str = "kernelcache.release.mac13j";
const KERNEL_CACHE_COMPRESSED_SIZE: u64 = 33_043_908;
const KERNEL_CACHE_ARCHIVE_SIZE: u64 = 33_096_787;
const KERNEL_CACHE_PAYLOAD_SIZE: usize = 33_096_384;
const KERNEL_CACHE_IMAGE_SIZE: usize = 124_354_560;
const FAIR_PLAY_KEXT_IDENTIFIER: &str = "com.apple.driver.FairPlayIOKit";
const MAXIMUM_KERNEL_CACHE_SYMBOLS: usize = 1 << 20;

const TAG_IA5_STRING: u8 = 0x16;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_SEQUENCE: u8 = 0x30;

pub(super) fn extract_fair_play_kext(archive: &mut Archive<'_>) -> Result<Vec<u8>> {
    let image = kernel_collection_image(archive)?;
    let collection =
        macho::File::parse(&image, 0).context("open pinned kernel collection Mach-O")?;
    if collection.image_type != macho::MH_FILESET {
        return Err(Error::msg("pinned kernel collection is not a fileset"));
    }
    let mut symbols: HashMap<String, u64> = HashMap::new();
    for entry_id in collection.fileset_entry_ids() {
        let Ok(member) = collection.fileset_member(&image, &entry_id) else {
            continue;
        };
        let Some(symtab) = member.symtab() else {
            continue;
        };
        for symbol in &symtab.symbols {
            if symbol.is_external() && !symbol.is_undefined() && symbol.value != 0 {
                symbols.insert(symbol.name.clone(), symbol.value);
                if symbols.len() > MAXIMUM_KERNEL_CACHE_SYMBOLS {
                    return Err(Error::msg("pinned kernel symbol table exceeds the limit"));
                }
            }
        }
    }
    let mut member = collection
        .fileset_member(&image, FAIR_PLAY_KEXT_IDENTIFIER)
        .context("find FairPlayIOKit in pinned kernel collection")?;
    let mut resolved = Vec::new();
    for symbol in member.symtab().map_or(&[][..], |symtab| &symtab.symbols) {
        if !symbol.is_undefined() || symbol.name.is_empty() {
            continue;
        }
        if let Some(&value) = symbols.get(&symbol.name) {
            resolved.push(Symbol {
                name: symbol.name.clone(),
                indirect_name: String::new(),
                n_type: N_ABS | N_EXT,
                section: 0,
                desc: symbol.desc,
                value,
            });
        }
    }
    let reader = macho::FileReader::new(&image, &collection);
    member
        .export(&reader, &resolved)
        .context("export FairPlayIOKit from pinned kernel collection")
}

fn kernel_collection_image(archive: &mut Archive<'_>) -> Result<Vec<u8>> {
    let entry = pinned_zip_entry(
        archive,
        KERNEL_CACHE_NAME,
        KERNEL_CACHE_COMPRESSED_SIZE,
        KERNEL_CACHE_ARCHIVE_SIZE,
    )?;
    let mut data = Vec::new();
    archive
        .by_index(entry.index)
        .context("open pinned kernel collection")?
        .take(KERNEL_CACHE_ARCHIVE_SIZE + 1)
        .read_to_end(&mut data)
        .context("read pinned kernel collection")?;
    if data.len() as u64 != KERNEL_CACHE_ARCHIVE_SIZE {
        return Err(Error::msg(format!(
            "pinned kernel collection has size {}, expected {KERNEL_CACHE_ARCHIVE_SIZE}",
            data.len()
        )));
    }
    let payload = im4p_payload(&data).context("decode pinned kernel collection")?;
    if payload.len() != KERNEL_CACHE_PAYLOAD_SIZE {
        return Err(Error::msg(format!(
            "pinned kernel payload has size {}, expected {KERNEL_CACHE_PAYLOAD_SIZE}",
            payload.len()
        )));
    }
    let mut image = vec![0u8; KERNEL_CACHE_IMAGE_SIZE + 1];
    let size = lzfse::decode_buffer(payload, &mut image)
        .ok()
        .filter(|size| *size != 0)
        .ok_or_else(|| Error::msg("decompress pinned kernel collection: invalid LZFSE stream"))?;
    if size != KERNEL_CACHE_IMAGE_SIZE {
        return Err(Error::msg(format!(
            "pinned kernel image has size {size}, expected {KERNEL_CACHE_IMAGE_SIZE}"
        )));
    }
    image.truncate(size);
    Ok(image)
}

/// One DER element: its tag, whether it is constructed, its contents, and what follows.
fn der_element(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = data.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 8 || rest.len() < count || rest[0] == 0 {
            return None;
        }
        let length = rest[..count]
            .iter()
            .fold(0usize, |length, byte| (length << 8) | usize::from(*byte));
        if length < 0x80 {
            return None;
        }
        (length, &rest[count..])
    };
    if tag & 0x1f == 0x1f || rest.len() < length {
        return None;
    }
    Some((tag, &rest[..length], &rest[length..]))
}

pub(super) fn im4p_payload(data: &[u8]) -> Result<&[u8]> {
    let invalid_container = || Error::msg("invalid IM4P container");
    let (tag, mut contents, rest) = der_element(data).ok_or_else(invalid_container)?;
    if !rest.is_empty() || tag != TAG_SEQUENCE {
        return Err(invalid_container());
    }
    let mut fields = Vec::with_capacity(4);
    while !contents.is_empty() && fields.len() < 4 {
        let (tag, value, rest) = der_element(contents)
            .ok_or_else(|| Error::msg("decode IM4P field: asn1: syntax error"))?;
        fields.push((tag, value));
        contents = rest;
    }
    match fields.as_slice() {
        [
            (TAG_IA5_STRING, b"IM4P"),
            (TAG_IA5_STRING, b"krnl"),
            _,
            (TAG_OCTET_STRING, payload),
        ] => Ok(payload),
        _ => Err(Error::msg("invalid IM4P kernel payload")),
    }
}
