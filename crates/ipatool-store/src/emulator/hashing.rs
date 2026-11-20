//! Go `hashing.go`: Swift's `Hasher` (SipHash-1-3 in a 72-byte ABI buffer), used by guest
//! dictionaries; no Store signature depends on it.

use super::{Guest, Result, Uc};

pub(crate) fn init_hasher(uc: &mut Uc, address: u64, seed: u64) -> Result<()> {
    let (mut a, mut b) = (0, 0);
    let parameters = uc.sti().symbol("_swift_stdlib_Hashing_parameters");
    if parameters != 0 {
        a = uc.read64(parameters)?;
        b = uc.read64(parameters + 8)?;
    }
    a ^= seed;
    let values = [
        0,
        0x736f_6d65_7073_6575 ^ a,
        0x646f_7261_6e64_6f6d ^ b,
        0x6c79_6765_6e65_7261 ^ a,
        0x7465_6462_7974_6573 ^ b,
        0,
        0,
        0,
        0,
    ];
    for (index, value) in values.into_iter().enumerate() {
        uc.write64(address + index as u64 * 8, value)?;
    }
    Ok(())
}

fn sip_round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13) ^ v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16) ^ v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21) ^ v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17) ^ v[2];
    v[2] = v[2].rotate_left(32);
}

pub(crate) fn hash_compress(uc: &mut Uc, address: u64, word: u64, last: bool) -> Result<u64> {
    let mut v = [0u64; 4];
    for (index, slot) in v.iter_mut().enumerate() {
        *slot = uc.read64(address + 8 + index as u64 * 8)?;
    }
    v[3] ^= word;
    sip_round(&mut v);
    v[0] ^= word;
    if last {
        v[2] ^= 255;
        for _ in 0..3 {
            sip_round(&mut v);
        }
    }
    for (index, &word) in v.iter().enumerate() {
        uc.write64(address + 8 + index as u64 * 8, word)?;
    }
    Ok(v[0] ^ v[1] ^ v[2] ^ v[3])
}

pub(crate) fn hash_word(uc: &mut Uc, address: u64, word: u64) -> Result<()> {
    let mut tail = uc.read64(address)?;
    let count = tail >> 56;
    let shift = (count & 7) * 8;
    let mut chunk = word;
    if shift == 0 {
        tail = tail.wrapping_add(8 << 56);
    } else {
        chunk = (tail & 0x00ff_ffff_ffff_ffff) | (word << shift);
        tail = ((count + 8) << 56) | (word >> (64 - shift));
    }
    uc.write64(address, tail)?;
    hash_compress(uc, address, chunk, false)?;
    Ok(())
}

pub(crate) fn hash_bytes(uc: &mut Uc, address: u64, data: &[u8]) -> Result<()> {
    for &byte in data {
        let tail = uc.read64(address)?;
        let count = tail >> 56;
        let shift = (count & 7) * 8;
        let word = (tail & 0x00ff_ffff_ffff_ffff) | (u64::from(byte) << shift);
        if shift == 56 {
            hash_compress(uc, address, word, false)?;
            uc.write64(address, (count + 1) << 56)?;
        } else {
            uc.write64(address, ((count + 1) << 56) | word)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::runtime::{HEAP_BASE, HEAP_SIZE, test_engine};

    /// SipHash-1-3 of "abc" with key 0, computed independently of the guest buffer layout.
    fn reference(data: &[u8], k0: u64, k1: u64) -> u64 {
        let mut v = [
            0x736f_6d65_7073_6575 ^ k0,
            0x646f_7261_6e64_6f6d ^ k1,
            0x6c79_6765_6e65_7261 ^ k0,
            0x7465_6462_7974_6573 ^ k1,
        ];
        let (chunks, rest) = data.as_chunks::<8>();
        for chunk in chunks {
            let m = u64::from_le_bytes(*chunk);
            v[3] ^= m;
            sip_round(&mut v);
            v[0] ^= m;
        }
        let mut last = (data.len() as u64) << 56;
        for (index, &byte) in rest.iter().enumerate() {
            last |= u64::from(byte) << (index * 8);
        }
        v[3] ^= last;
        sip_round(&mut v);
        v[0] ^= last;
        v[2] ^= 255;
        for _ in 0..3 {
            sip_round(&mut v);
        }
        v[0] ^ v[1] ^ v[2] ^ v[3]
    }

    #[test]
    fn matches_siphash_1_3() {
        let mut uc = test_engine();
        uc.map_mem(HEAP_BASE, HEAP_SIZE).unwrap();
        for data in [&b"abc"[..], b"0123456789abcdef!"] {
            let hasher = uc.alloc(72).unwrap();
            init_hasher(&mut uc, hasher, 7).unwrap();
            hash_bytes(&mut uc, hasher, data).unwrap();
            let tail = uc.read64(hasher).unwrap();
            let hash = hash_compress(&mut uc, hasher, tail, true).unwrap();
            assert_eq!(hash, reference(data, 7, 0));
        }
        let hasher = uc.alloc(72).unwrap();
        init_hasher(&mut uc, hasher, 0).unwrap();
        hash_word(&mut uc, hasher, 0x0807_0605_0403_0201).unwrap();
        let tail = uc.read64(hasher).unwrap();
        let hash = hash_compress(&mut uc, hasher, tail, true).unwrap();
        assert_eq!(hash, reference(&[1, 2, 3, 4, 5, 6, 7, 8], 0, 0));
    }
}
