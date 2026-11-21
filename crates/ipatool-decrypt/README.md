# ipatool-decrypt

Offline FairPlay decryption of App Store IPAs.

This crate is a Rust port of [EthanArbuckle/fairplay-decrypt](https://github.com/EthanArbuckle/fairplay-decrypt)
(`core.py` and `decrypt.py`), which is licensed under the GNU General Public License version 3
**"or any later version"**. This port is therefore licensed **GPL-3.0-or-later** (see
[`LICENSE`](LICENSE)), unlike the MIT workspace it lives in.

It derives per-page keys from an account's 16-byte credential and an app's SINF and SUPF
records, decrypts the encrypted arm64 Mach-O pages (AES-128-CBC) and clears `cryptid`. Only
offline decryption is ported; credential minting and on-device decryption are out of scope.

A binary that links this crate (the `ipatool-cli` `decrypt` feature) is GPL-3.
