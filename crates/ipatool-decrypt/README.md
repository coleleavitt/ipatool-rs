# ipatool-decrypt

Offline FairPlay decryption of App Store IPAs.

This crate is a Rust port of ipadecrypt's `internal/fairplay`
([londek/ipadecrypt](https://github.com/londek/ipadecrypt), MIT; see
[`LICENSE-ipadecrypt`](LICENSE-ipadecrypt)), which is a Go port of
[EthanArbuckle/fairplay-decrypt](https://github.com/EthanArbuckle/fairplay-decrypt)
(`core.py` and `decrypt.py`). fairplay-decrypt is licensed under the GNU General Public License
version 3 **"or any later version"**, and this crate began as a port of it, so it is licensed
**GPL-3.0-or-later** (see [`LICENSE`](LICENSE)), unlike the MIT workspace it lives in.

It derives per-page keys from an account's 16-byte credential (or its Arcade credential, for
Apple Arcade licenses) and an app's SINF and SUPF/SUPP records, decrypts one ARM slice of each
encrypted Mach-O (AES-128-CBC), thins the binary to that slice, clears `cryptid`, and checks
that no binary in the output is left encrypted. Credential minting and on-device decryption
are not part of the offline decryptor.

Test fixtures are synthetic (no real credential). `tests/fixtures/decrypt_binary*.json` come
from fairplay-decrypt's `core.py`; the other fixtures come from ipadecrypt through
`tests/go_fixtures/gen_test.go` (see its header for how to regenerate them).

A binary that links this crate (the `ipatool-cli` `decrypt` feature) is GPL-3.
