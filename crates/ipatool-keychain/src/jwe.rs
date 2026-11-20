//! The compact JWE that `jose2go` produces for the Go file keyring:
//! `PBES2-HS256+A128KW` key wrapping (PBKDF2-SHA256, 16-byte KEK, AES key wrap) and
//! `A256GCM` content encryption, with the encoded header as additional data.

use std::collections::BTreeMap;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use aes_kw::KwAes128;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use sha2::Sha256;

use crate::{Error, Result};

const ALG: &str = "PBES2-HS256+A128KW";
const ENC: &str = "A256GCM";
/// jose2go's default iteration count for new tokens.
const ITERATIONS: u32 = 8192;
/// jose2go's upper bound for `p2c` when decoding with the default registration.
const MAX_ITERATIONS: u64 = 1_300_000;
const CEK_LEN: usize = 32;
const TAG_LEN: usize = 16;

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|err| Error::Random(err.to_string()))?;
    Ok(bytes)
}

fn kek(passphrase: &str, salt_input: &[u8], iterations: u32) -> [u8; 16] {
    let mut salt = Vec::with_capacity(ALG.len() + 1 + salt_input.len());
    salt.extend_from_slice(ALG.as_bytes());
    salt.push(0);
    salt.extend_from_slice(salt_input);
    pbkdf2::pbkdf2_hmac_array::<Sha256, 16>(passphrase.as_bytes(), &salt, iterations)
}

/// Encrypts `payload`. `created` goes into the header like Go's `time.Now().String()`.
pub(crate) fn encrypt(payload: &[u8], passphrase: &str, created: &str) -> Result<String> {
    let salt_input = random::<12>()?;
    let cek = random::<CEK_LEN>()?;
    let iv = random::<12>()?;

    // Go marshals the header map with sorted keys.
    let header: BTreeMap<&str, Value> = BTreeMap::from([
        ("alg", ALG.into()),
        ("created", created.into()),
        ("enc", ENC.into()),
        ("p2c", ITERATIONS.into()),
        ("p2s", URL_SAFE_NO_PAD.encode(salt_input).into()),
    ]);
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);

    let wrap = KwAes128::new_from_slice(&kek(passphrase, &salt_input, ITERATIONS))
        .map_err(|_| Error::Token("key size"))?;
    let mut wrapped = [0u8; CEK_LEN + 8];
    wrap.wrap_key(&cek, &mut wrapped)
        .map_err(|_| Error::Token("key wrap"))?;

    let cipher = Aes256Gcm::new_from_slice(&cek).map_err(|_| Error::Token("key size"))?;
    let sealed = cipher
        .encrypt(
            &Nonce::from(iv),
            Payload {
                msg: payload,
                aad: header.as_bytes(),
            },
        )
        .map_err(|_| Error::Token("encryption failed"))?;
    let (ciphertext, tag) = sealed.split_at(sealed.len() - TAG_LEN);

    Ok([
        header,
        URL_SAFE_NO_PAD.encode(wrapped),
        URL_SAFE_NO_PAD.encode(iv),
        URL_SAFE_NO_PAD.encode(ciphertext),
        URL_SAFE_NO_PAD.encode(tag),
    ]
    .join("."))
}

pub(crate) fn decrypt(token: &str, passphrase: &str) -> Result<Vec<u8>> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    let [header_b64, wrapped, iv, ciphertext, tag] = parts[..] else {
        return Err(Error::Token("expected five parts"));
    };
    let decode = |part: &str| {
        URL_SAFE_NO_PAD
            .decode(part)
            .map_err(|_| Error::Token("bad base64"))
    };

    let header: serde_json::Map<String, Value> = serde_json::from_slice(&decode(header_b64)?)?;
    if header.get("alg").and_then(Value::as_str) != Some(ALG)
        || header.get("enc").and_then(Value::as_str) != Some(ENC)
    {
        return Err(Error::Token("unsupported algorithm"));
    }
    let iterations = header
        .get("p2c")
        .and_then(Value::as_u64)
        .filter(|&count| count <= MAX_ITERATIONS)
        .and_then(|count| u32::try_from(count).ok())
        .ok_or(Error::Token("bad p2c"))?;
    let salt_input = decode(
        header
            .get("p2s")
            .and_then(Value::as_str)
            .ok_or(Error::Token("missing p2s"))?,
    )?;

    let wrap = KwAes128::new_from_slice(&kek(passphrase, &salt_input, iterations))
        .map_err(|_| Error::Token("key size"))?;
    let wrapped = decode(wrapped)?;
    let mut cek = vec![0u8; wrapped.len().saturating_sub(8)];
    let cek = wrap
        .unwrap_key(&wrapped, &mut cek)
        .map_err(|_| Error::Decrypt)?;

    let iv: [u8; 12] = decode(iv)?.try_into().map_err(|_| Error::Token("bad iv"))?;
    let mut sealed = decode(ciphertext)?;
    sealed.extend(decode(tag)?);

    let cipher = Aes256Gcm::new_from_slice(cek).map_err(|_| Error::Token("key size"))?;
    cipher
        .decrypt(
            &Nonce::from(iv),
            Payload {
                msg: &sealed,
                aad: header_b64.as_bytes(),
            },
        )
        .map_err(|_| Error::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let token = encrypt(b"secret", "pass", "now").unwrap();
        assert_eq!(decrypt(&token, "pass").unwrap(), b"secret");
        assert!(matches!(decrypt(&token, "wrong"), Err(Error::Decrypt)));
    }
}
