//! Port of Go `assets/aea.go`: random access to an Apple Encrypted Archive whose key is
//! published through Apple's FCS key service.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use aes::Aes256;
use base64::Engine as _;
use ctr::Ctr128BE;
use ctr::cipher::{KeyIvInit, StreamCipher};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use hpke::aead::AesGcm256;
use hpke::kdf::HkdfSha256;
use hpke::kem::DhP256HkdfSha256;
use hpke::{Deserializable, OpModeR};
use p256::pkcs8::DecodePrivateKey;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};

use super::{ReadAt, le32, le64};
use crate::error::{Context, Error, Result};

const AEA_MAGIC: &[u8; 4] = b"AEA1";
const AEA_SYMMETRIC_PROFILE: u32 = 1;
const AEA_MAIN_KEY_INFO: &str = "AEA_AMK";
const AEA_ROOT_KEY_INFO: &str = "AEA_RHEK";
const AEA_CLUSTER_KEY_INFO: &str = "AEA_CK";
const AEA_CLUSTER_HEADER_KEY_INFO: &str = "AEA_CHEK";
const AEA_SEGMENT_KEY_INFO: &str = "AEA_SK";
const AEA_LZFSE_COMPRESSION: u8 = b'e';
const AEA_SHA256_CHECKSUM: u8 = 2;
const AEA_DECODED_SEGMENT_CACHE_COUNT: usize = 64;
const MAX_AEA_AUTH_DATA_SIZE: u32 = 1 << 20;
const MAX_FCS_KEY_SIZE: u64 = 64 << 10;

const AEA_HEADER_SIZE: usize = 12;
const ENCRYPTED_ROOT_HEADER_SIZE: usize = 32 + 48 + 32;
const SEGMENT_HEADER_SIZE: usize = 8 + 32;

type AeaMac = [u8; 32];

struct HeaderKey {
    mac: [u8; 32],
    key: [u8; 32],
    iv: [u8; 16],
}

struct RootHeader {
    file_size: u64,
    encrypted_size: u64,
    segment_size: u32,
    segments_per_cluster: u32,
    compression: u8,
    checksum: u8,
}

#[derive(Clone, Copy, Default)]
struct Segment {
    input_offset: u64,
    output_offset: u64,
    compressed_size: u32,
    decompressed_size: u32,
    checksum: [u8; 32],
    mac: AeaMac,
    cluster: u32,
    index: u32,
}

pub(super) struct AeaReader<'a> {
    source: &'a dyn ReadAt,
    size: u64,
    segment_size: u32,
    main_key: Vec<u8>,
    segments: Vec<Segment>,
    cache: Mutex<SegmentCache>,
}

#[derive(Default)]
struct SegmentCache {
    decoded: HashMap<usize, std::sync::Arc<Vec<u8>>>,
    order: VecDeque<usize>,
}

#[derive(serde::Deserialize)]
struct FcsResponse {
    #[serde(rename = "enc-request", default)]
    enc_request: String,
    #[serde(rename = "wrapped-key", default)]
    wrapped_key: String,
}

impl<'a> AeaReader<'a> {
    pub(super) fn open(client: &Client, source: &'a dyn ReadAt, source_size: u64) -> Result<Self> {
        let mut header = [0u8; AEA_HEADER_SIZE];
        source
            .read_at(&mut header, 0)
            .context("read binary value")
            .context("read AEA header")?;
        if &header[..4] != AEA_MAGIC {
            return Err(Error::msg(format!(
                "invalid AEA magic {:?}",
                String::from_utf8_lossy(&header[..4])
            )));
        }
        let profile_and_strength = le32(&header[4..]);
        if profile_and_strength & 0x00ff_ffff != AEA_SYMMETRIC_PROFILE {
            return Err(Error::msg(format!(
                "unsupported AEA profile {}",
                profile_and_strength & 0x00ff_ffff
            )));
        }
        let auth_data_length = le32(&header[8..]);
        if auth_data_length > MAX_AEA_AUTH_DATA_SIZE {
            return Err(Error::msg(format!(
                "AEA authentication data is too large: {auth_data_length}"
            )));
        }
        let mut auth_data = vec![0u8; auth_data_length as usize];
        source
            .read_at(&mut auth_data, AEA_HEADER_SIZE as u64)
            .context("read AEA authentication data")?;
        let metadata = parse_aea_metadata(&auth_data)?;
        let symmetric_key = resolve_aea_symmetric_key(client, &metadata)?;

        let main_salt_offset = AEA_HEADER_SIZE as u64 + u64::from(auth_data_length);
        let mut main_salt = [0u8; 32];
        source
            .read_at(&mut main_salt, main_salt_offset)
            .context("read AEA main salt")?;
        let main_key = derive_aea_key(
            &symmetric_key,
            &main_salt,
            &info_with_index(AEA_MAIN_KEY_INFO, profile_and_strength),
            symmetric_key.len(),
        )
        .context("derive AEA main key")?;

        let root_offset = main_salt_offset + 32;
        let mut encrypted_root = [0u8; ENCRYPTED_ROOT_HEADER_SIZE];
        source
            .read_at(&mut encrypted_root, root_offset)
            .context("read binary value")
            .context("read encrypted AEA root header")?;
        let (root_mac, rest) = encrypted_root.split_at(32);
        let (root_data, cluster_mac) = rest.split_at(48);
        let root_key = derive_aea_header_key(&main_key, AEA_ROOT_KEY_INFO.as_bytes())
            .context("derive AEA root key")?;
        let mut root_salt = Vec::with_capacity(32 + auth_data.len());
        root_salt.extend_from_slice(cluster_mac);
        root_salt.extend_from_slice(&auth_data);
        if !mac_equal(
            root_mac,
            &calculate_aea_mac(&root_key.mac, root_data, &root_salt),
        ) {
            return Err(Error::msg("AEA root header failed authentication"));
        }
        let root_data = decrypt_aea_ctr(root_data, &root_key.key, root_key.iv);
        let root = RootHeader {
            file_size: le64(&root_data),
            encrypted_size: le64(&root_data[8..]),
            segment_size: le32(&root_data[16..]),
            segments_per_cluster: le32(&root_data[20..]),
            compression: root_data[24],
            checksum: root_data[25],
        };
        if root.file_size == 0 || root.file_size > u64::MAX >> 1 {
            return Err(Error::msg(format!(
                "invalid AEA output size {}",
                root.file_size
            )));
        }
        if root.encrypted_size != source_size {
            return Err(Error::msg(format!(
                "AEA encrypted size is {}, expected {source_size}",
                root.encrypted_size
            )));
        }
        if root.segment_size == 0 || root.segments_per_cluster == 0 {
            return Err(Error::msg("AEA has an invalid segment layout"));
        }
        if root.compression != AEA_LZFSE_COMPRESSION || root.checksum != AEA_SHA256_CHECKSUM {
            return Err(Error::msg(format!(
                "unsupported AEA encoding: compression {:?}, checksum {}",
                char::from(root.compression),
                root.checksum
            )));
        }
        let segment_count = root.file_size.div_ceil(u64::from(root.segment_size));
        let mut reader = Self {
            source,
            size: root.file_size,
            segment_size: root.segment_size,
            main_key,
            segments: vec![
                Segment::default();
                usize::try_from(segment_count)
                    .map_err(|_| Error::msg("AEA has an invalid segment layout"))?
            ],
            cache: Mutex::new(SegmentCache::default()),
        };
        let mut first_cluster_mac = [0u8; 32];
        first_cluster_mac.copy_from_slice(cluster_mac);
        reader.index_clusters(
            root_offset + ENCRYPTED_ROOT_HEADER_SIZE as u64,
            first_cluster_mac,
            &root,
            source_size,
        )?;
        Ok(reader)
    }

    pub(super) fn size(&self) -> u64 {
        self.size
    }

    fn index_clusters(
        &mut self,
        mut offset: u64,
        mut cluster_mac: AeaMac,
        root: &RootHeader,
        source_size: u64,
    ) -> Result<()> {
        let per_cluster = u64::from(root.segments_per_cluster);
        let header_size = SEGMENT_HEADER_SIZE as u64 * per_cluster + 32 + 32 * per_cluster;
        let mut output_size = 0u64;
        let mut cluster_index = 0u32;
        while output_size < root.file_size {
            if header_size > source_size.saturating_sub(offset) {
                return Err(Error::msg(
                    "AEA cluster header extends beyond the encrypted image",
                ));
            }
            let mut header_data = vec![0u8; to_usize(header_size)?];
            self.source
                .read_at(&mut header_data, offset)
                .with_context(|| format!("read AEA cluster {cluster_index} header"))?;
            let cluster_key = derive_aea_key(
                &self.main_key,
                &[],
                &info_with_index(AEA_CLUSTER_KEY_INFO, cluster_index),
                self.main_key.len(),
            )
            .with_context(|| format!("derive AEA cluster {cluster_index} key"))?;
            let cluster_header_key =
                derive_aea_header_key(&cluster_key, AEA_CLUSTER_HEADER_KEY_INFO.as_bytes())
                    .with_context(|| format!("derive AEA cluster {cluster_index} header key"))?;

            let encrypted_headers_size = to_usize(SEGMENT_HEADER_SIZE as u64 * per_cluster)?;
            let (encrypted_headers, macs) = header_data.split_at(encrypted_headers_size);
            let (next_cluster_mac, segment_mac_data) = macs.split_at(32);
            let mut hmac_salt = Vec::with_capacity(macs.len());
            hmac_salt.extend_from_slice(macs);
            if !mac_equal(
                &cluster_mac,
                &calculate_aea_mac(&cluster_header_key.mac, encrypted_headers, &hmac_salt),
            ) {
                return Err(Error::msg(format!(
                    "AEA cluster {cluster_index} failed authentication"
                )));
            }
            let decrypted_headers = decrypt_aea_ctr(
                encrypted_headers,
                &cluster_header_key.key,
                cluster_header_key.iv,
            );

            let mut data_offset = offset + header_size;
            for segment_index in 0..root.segments_per_cluster {
                let start = segment_index as usize * SEGMENT_HEADER_SIZE;
                let header = &decrypted_headers[start..start + SEGMENT_HEADER_SIZE];
                let decompressed_size = le32(header);
                let compressed_size = le32(&header[4..]);
                if decompressed_size == 0 {
                    if compressed_size != 0 {
                        return Err(Error::msg(format!(
                            "AEA cluster {cluster_index} segment {segment_index} has compressed data without output"
                        )));
                    }
                    continue;
                }
                if decompressed_size > root.segment_size || compressed_size == 0 {
                    return Err(Error::msg(format!(
                        "AEA cluster {cluster_index} segment {segment_index} has invalid sizes"
                    )));
                }
                if u64::from(compressed_size) > source_size.saturating_sub(data_offset) {
                    return Err(Error::msg(format!(
                        "AEA cluster {cluster_index} segment {segment_index} extends beyond the encrypted image"
                    )));
                }
                let mac_start = segment_index as usize * 32;
                let mut mac = [0u8; 32];
                mac.copy_from_slice(&segment_mac_data[mac_start..mac_start + 32]);
                let global_index =
                    u64::from(cluster_index) * per_cluster + u64::from(segment_index);
                let Some(slot) = usize::try_from(global_index)
                    .ok()
                    .and_then(|index| self.segments.get_mut(index))
                else {
                    return Err(Error::msg(format!(
                        "AEA cluster {cluster_index} segment {segment_index} is outside the declared output"
                    )));
                };
                let output_offset = global_index * u64::from(root.segment_size);
                if output_offset + u64::from(decompressed_size) > root.file_size {
                    return Err(Error::msg(format!(
                        "AEA cluster {cluster_index} segment {segment_index} extends beyond the declared output"
                    )));
                }
                let mut checksum = [0u8; 32];
                checksum.copy_from_slice(&header[8..40]);
                *slot = Segment {
                    input_offset: data_offset,
                    output_offset,
                    compressed_size,
                    decompressed_size,
                    checksum,
                    mac,
                    cluster: cluster_index,
                    index: segment_index,
                };
                data_offset += u64::from(compressed_size);
                output_size += u64::from(decompressed_size);
                if output_size > root.file_size {
                    return Err(Error::msg(
                        "AEA segment data exceeds the declared output size",
                    ));
                }
            }
            offset = data_offset;
            cluster_mac.copy_from_slice(next_cluster_mac);
            cluster_index += 1;
        }
        if output_size != root.file_size {
            return Err(Error::msg(format!(
                "AEA segment data has size {output_size}, expected {}",
                root.file_size
            )));
        }
        Ok(())
    }

    fn decode_segment(
        &self,
        cache_index: usize,
        segment: &Segment,
    ) -> Result<std::sync::Arc<Vec<u8>>> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(data) = cache.decoded.get(&cache_index) {
            return Ok(data.clone());
        }
        let (cluster, index) = (segment.cluster, segment.index);
        let mut encrypted = vec![0u8; segment.compressed_size as usize];
        self.source
            .read_at(&mut encrypted, segment.input_offset)
            .with_context(|| format!("read AEA cluster {cluster} segment {index}"))?;
        let cluster_key = derive_aea_key(
            &self.main_key,
            &[],
            &info_with_index(AEA_CLUSTER_KEY_INFO, cluster),
            self.main_key.len(),
        )
        .with_context(|| format!("derive AEA cluster {cluster} key"))?;
        let segment_key =
            derive_aea_header_key(&cluster_key, &info_with_index(AEA_SEGMENT_KEY_INFO, index))
                .with_context(|| format!("derive AEA cluster {cluster} segment {index} key"))?;
        if !mac_equal(
            &segment.mac,
            &calculate_aea_mac(&segment_key.mac, &encrypted, &[]),
        ) {
            return Err(Error::msg(format!(
                "AEA cluster {cluster} segment {index} failed authentication"
            )));
        }
        let compressed = decrypt_aea_ctr(&encrypted, &segment_key.key, segment_key.iv);
        let decoded = if segment.compressed_size == segment.decompressed_size {
            compressed
        } else {
            let expected = segment.decompressed_size as usize;
            let decoded = super::lzfse_decode(&compressed, expected)
                .with_context(|| format!("decompress AEA cluster {cluster} segment {index}"))?;
            if decoded.len() != expected {
                return Err(Error::msg(format!(
                    "decompress AEA cluster {cluster} segment {index}: produced {} bytes, expected {expected}",
                    decoded.len()
                )));
            }
            decoded
        };
        if Sha256::digest(&decoded)[..] != segment.checksum {
            return Err(Error::msg(format!(
                "AEA cluster {cluster} segment {index} failed checksum verification"
            )));
        }
        if cache.order.len() == AEA_DECODED_SEGMENT_CACHE_COUNT
            && let Some(oldest) = cache.order.pop_front()
        {
            cache.decoded.remove(&oldest);
        }
        let decoded = std::sync::Arc::new(decoded);
        cache.decoded.insert(cache_index, decoded.clone());
        cache.order.push_back(cache_index);
        Ok(decoded)
    }
}

impl ReadAt for AeaReader<'_> {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if offset >= self.size || data.len() as u64 > self.size - offset {
            return Err(Error::msg("EOF"));
        }
        let mut written = 0;
        while written < data.len() {
            let position = offset + written as u64;
            let segment_index = to_usize(position / u64::from(self.segment_size))?;
            let Some(segment) = self.segments.get(segment_index) else {
                return Err(Error::msg("AEA segment index is incomplete"));
            };
            if segment.decompressed_size == 0 {
                return Err(Error::msg(format!(
                    "AEA segment {segment_index} is missing"
                )));
            }
            let decoded = self.decode_segment(segment_index, segment)?;
            let inside = to_usize(position - segment.output_offset)?;
            if inside >= decoded.len() {
                return Err(Error::msg("AEA segment has an invalid output offset"));
            }
            let count = (data.len() - written).min(decoded.len() - inside);
            data[written..written + count].copy_from_slice(&decoded[inside..inside + count]);
            written += count;
        }
        Ok(())
    }
}

pub(super) fn parse_aea_metadata(mut data: &[u8]) -> Result<HashMap<String, Vec<u8>>> {
    let mut metadata = HashMap::new();
    while !data.is_empty() {
        if data.len() < 4 {
            return Err(Error::msg("read AEA metadata length: unexpected EOF"));
        }
        let length = le32(data);
        data = &data[4..];
        if length < 5 || u64::from(length - 4) > data.len() as u64 {
            return Err(Error::msg(format!(
                "invalid AEA metadata record length {length}"
            )));
        }
        let (record, rest) = data.split_at(length as usize - 4);
        data = rest;
        let Some(separator) = record.iter().position(|byte| *byte == 0) else {
            return Err(Error::msg("invalid AEA metadata record"));
        };
        if separator == 0 {
            return Err(Error::msg("invalid AEA metadata record"));
        }
        metadata.insert(
            String::from_utf8_lossy(&record[..separator]).into_owned(),
            record[separator + 1..].to_vec(),
        );
    }
    Ok(metadata)
}

/// Downloads the Apple-published FCS P-256 private key named by the AEA metadata. It is
/// the HPKE recipient key for this image's wrapped symmetric key: restore-image key
/// material, not a user credential.
fn resolve_aea_symmetric_key(
    client: &Client,
    metadata: &HashMap<String, Vec<u8>>,
) -> Result<Vec<u8>> {
    let key_url_data = metadata
        .get("com.apple.wkms.fcs-key-url")
        .ok_or_else(|| Error::msg("AEA metadata does not contain an FCS key URL"))?;
    let key_url_text = String::from_utf8_lossy(key_url_data);
    let key_url = url::Url::parse(&key_url_text)
        .ok()
        .filter(|url| url.scheme() == "https" && url.host_str() == Some("wkms-public.apple.com"))
        .ok_or_else(|| Error::msg(format!("invalid Apple FCS key URL {key_url_text:?}")))?;
    let response = client
        .get(key_url.as_str())
        .send()
        .context("download Apple FCS key")?;
    if response.status() != StatusCode::OK {
        return Err(Error::msg(format!(
            "download Apple FCS key: server returned {}",
            response.status()
        )));
    }
    let mut pem_data = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(response, MAX_FCS_KEY_SIZE + 1),
        &mut pem_data,
    )
    .context("read Apple FCS key")?;
    if pem_data.len() as u64 > MAX_FCS_KEY_SIZE {
        return Err(Error::msg("apple FCS key response is too large"));
    }
    let pem_text = std::str::from_utf8(&pem_data)
        .map_err(|_| Error::msg("apple FCS key response is not PEM encoded"))?;
    let private_key = p256::SecretKey::from_pkcs8_pem(pem_text)
        .map_err(|error| Error::msg(format!("parse Apple FCS private key: {error}")))?;

    let response_data = metadata
        .get("com.apple.wkms.fcs-response")
        .ok_or_else(|| Error::msg("AEA metadata does not contain an FCS response"))?;
    let wrapped: FcsResponse =
        serde_json::from_slice(response_data).context("parse AEA FCS response")?;
    let engine = base64::engine::general_purpose::STANDARD;
    let encapsulation = engine
        .decode(&wrapped.enc_request)
        .map_err(|error| Error::msg(format!("decode AEA FCS encapsulation: {error}")))?;
    let wrapped_key = engine
        .decode(&wrapped.wrapped_key)
        .map_err(|error| Error::msg(format!("decode AEA wrapped key: {error}")))?;

    let recipient_key =
        <DhP256HkdfSha256 as hpke::Kem>::PrivateKey::from_bytes(&private_key.to_bytes())
            .map_err(|error| Error::msg(format!("load AEA FCS private key: {error}")))?;
    let encapsulated = <DhP256HkdfSha256 as hpke::Kem>::EncappedKey::from_bytes(&encapsulation)
        .map_err(|error| Error::msg(format!("initialize AEA FCS recipient: {error}")))?;
    hpke::single_shot_open::<AesGcm256, HkdfSha256, DhP256HkdfSha256>(
        &OpModeR::Base,
        &recipient_key,
        &encapsulated,
        &[],
        &wrapped_key,
        &[],
    )
    .map_err(|error| Error::msg(format!("unwrap AEA symmetric key: {error}")))
}

fn info_with_index(info: &str, index: u32) -> Vec<u8> {
    let mut data = info.as_bytes().to_vec();
    data.extend_from_slice(&index.to_le_bytes());
    data
}

fn derive_aea_key(key: &[u8], salt: &[u8], info: &[u8], size: usize) -> Result<Vec<u8>> {
    let mut derived = vec![0u8; size];
    Hkdf::<Sha256>::new(Some(salt), key)
        .expand(info, &mut derived)
        .map_err(|error| Error::msg(format!("expand AEA key: {error}")))?;
    Ok(derived)
}

fn derive_aea_header_key(key: &[u8], info: &[u8]) -> Result<HeaderKey> {
    let derived = derive_aea_key(key, &[], info, 80).context("derive AEA header key")?;
    let mut header_key = HeaderKey {
        mac: [0; 32],
        key: [0; 32],
        iv: [0; 16],
    };
    header_key.mac.copy_from_slice(&derived[..32]);
    header_key.key.copy_from_slice(&derived[32..64]);
    header_key.iv.copy_from_slice(&derived[64..]);
    Ok(header_key)
}

fn calculate_aea_mac(key: &[u8], data: &[u8], salt: &[u8]) -> AeaMac {
    let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(key) else {
        unreachable!("HMAC accepts keys of any length");
    };
    mac.update(salt);
    mac.update(data);
    mac.update(&(salt.len() as u64).to_le_bytes());
    mac.finalize().into_bytes().into()
}

fn mac_equal(expected: &[u8], computed: &AeaMac) -> bool {
    expected.len() == computed.len()
        && expected
            .iter()
            .zip(computed)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

fn decrypt_aea_ctr(data: &[u8], key: &[u8; 32], iv: [u8; 16]) -> Vec<u8> {
    let mut decrypted = data.to_vec();
    Ctr128BE::<Aes256>::new(key.into(), (&iv).into()).apply_keystream(&mut decrypted);
    decrypted
}

fn to_usize(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| Error::msg("AEA offset overflows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(key: &str, value: &str) -> Vec<u8> {
        let mut record = key.as_bytes().to_vec();
        record.push(0);
        record.extend_from_slice(value.as_bytes());
        let mut data = u32::try_from(record.len() + 4)
            .unwrap()
            .to_le_bytes()
            .to_vec();
        data.extend_from_slice(&record);
        data
    }

    #[test]
    fn parses_bounded_aea_metadata_records() {
        let mut data = record("first", "value");
        data.extend_from_slice(&record("second", "another"));
        let metadata = parse_aea_metadata(&data).unwrap();
        assert_eq!(metadata["first"], b"value");
        assert_eq!(metadata["second"], b"another");
    }

    #[test]
    fn rejects_an_aea_metadata_record_outside_the_authentication_block() {
        let mut data = vec![0u8; 8];
        data[..4].copy_from_slice(&32u32.to_le_bytes());
        let error = parse_aea_metadata(&data).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("invalid AEA metadata record length"),
            "{error}"
        );
    }
}
