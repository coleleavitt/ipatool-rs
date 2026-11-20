//! The `FairPlay` device-key AES service, evaluated without the host operating system.

use aes::Aes128;
use aes::cipher::{Array, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};

use crate::error::{Error, Result};

pub(crate) const COMMERCE_AES_HANDLE: u32 = 0x89c;
const BLOCK_SIZE: usize = 16;

/// Go's `emulateIOAES`: AES-128-CBC under the all-zero device key, returning the output and
/// the chaining value the hardware leaves in the IV buffer.
pub(crate) fn emulate_io_aes(
    source: &[u8],
    iv: &[u8],
    operation: u32,
    key_data: &[u8],
) -> Result<(Vec<u8>, Vec<u8>)> {
    if source.is_empty()
        || !source.len().is_multiple_of(BLOCK_SIZE)
        || iv.len() != BLOCK_SIZE
        || key_data.len() != 40
    {
        return Err(Error::msg("invalid IOAES parameters"));
    }
    let handle = super::shims::le32(&key_data[36..40]);
    if handle != COMMERCE_AES_HANDLE {
        return Err(Error::msg(format!(
            "unsupported IOAES key handle {handle:#x}"
        )));
    }
    let cipher = Aes128::new(&Array::from([0u8; BLOCK_SIZE]));
    let mut chain = [0u8; BLOCK_SIZE];
    chain.copy_from_slice(iv);
    let mut destination = Vec::with_capacity(source.len());
    match operation {
        0 => {
            for block in source.as_chunks::<BLOCK_SIZE>().0 {
                let mut value = Array::from(chain);
                for (byte, input) in value.iter_mut().zip(block) {
                    *byte ^= input;
                }
                cipher.encrypt_block(&mut value);
                chain.copy_from_slice(&value);
                destination.extend_from_slice(&value);
            }
            Ok((destination, chain.to_vec()))
        }
        1 => {
            for block in source.as_chunks::<BLOCK_SIZE>().0 {
                let mut value = Array::from([0u8; BLOCK_SIZE]);
                value.copy_from_slice(block);
                cipher.decrypt_block(&mut value);
                for (byte, previous) in value.iter_mut().zip(chain) {
                    *byte ^= previous;
                }
                chain.copy_from_slice(block);
                destination.extend_from_slice(&value);
            }
            Ok((destination, chain.to_vec()))
        }
        _ => Err(Error::msg(format!(
            "unsupported IOAES operation {operation:#x}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_data(handle: u32) -> Vec<u8> {
        let mut key = vec![0; 40];
        key[36..40].copy_from_slice(&handle.to_le_bytes());
        key
    }

    #[test]
    fn evaluates_commerce_handle_with_emulated_key() {
        let key = key_data(COMMERCE_AES_HANDLE);
        let plaintext = [0u8; 16];
        let expected = [
            0x66, 0xe9, 0x4b, 0xd4, 0xef, 0x8a, 0x2c, 0x3b, 0x88, 0x4c, 0xfa, 0x59, 0xca, 0x34,
            0x2b, 0x2e,
        ];
        let (ciphertext, iv) = emulate_io_aes(&plaintext, &[0; 16], 0, &key).unwrap();
        assert_eq!(ciphertext, expected);
        assert_eq!(iv, expected);
        let (decrypted, iv) = emulate_io_aes(&ciphertext, &[0; 16], 1, &key).unwrap();
        assert_eq!(decrypted, plaintext);
        assert_eq!(iv, ciphertext);
    }

    #[test]
    fn rejects_unrelated_key_handles() {
        let error = emulate_io_aes(&[0; 16], &[0; 16], 0, &key_data(1)).unwrap_err();
        assert_eq!(error.to_string(), "unsupported IOAES key handle 0x1");
    }
}
