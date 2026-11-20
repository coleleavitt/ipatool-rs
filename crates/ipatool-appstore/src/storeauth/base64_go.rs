//! Go's `base64.StdEncoding` decoder. It differs from the `base64` crate's strict engine: it
//! skips `\r` and `\n`, accepts non-zero trailing bits, and on corrupt input still returns the
//! bytes of the quanta before the error (the Store keybag path keeps them).

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn sextet(byte: u8) -> Option<u32> {
    ALPHABET
        .iter()
        .position(|&c| c == byte)
        .and_then(|index| u32::try_from(index).ok())
}

/// `base64.StdEncoding.DecodeString`: the decoded bytes and whether the input was valid.
pub(super) fn decode(input: &str) -> (Vec<u8>, bool) {
    let symbols: Vec<u8> = input
        .bytes()
        .filter(|&byte| byte != b'\r' && byte != b'\n')
        .collect();
    let mut output = Vec::with_capacity(symbols.len() / 4 * 3);
    let mut quanta = symbols.chunks(4).peekable();
    while let Some(quantum) = quanta.next() {
        let last = quanta.peek().is_none();
        let Some(value) = quantum_value(quantum) else {
            return (output, false);
        };
        let (bits, length) = value;
        output.extend_from_slice(&bits.to_be_bytes()[1..=length]);
        if length < 3 {
            // Padding ends the data; anything after it is corrupt.
            return (output, last);
        }
    }
    (output, true)
}

/// One four-symbol quantum as 24 bits and its decoded length (3, or 1–2 when padded).
fn quantum_value(quantum: &[u8]) -> Option<(u32, usize)> {
    let [a, b, c, d] = *quantum else {
        return None;
    };
    let (a, b) = (sextet(a)?, sextet(b)?);
    let high = (a << 18) | (b << 12);
    match (c, d) {
        (b'=', b'=') => Some((high, 1)),
        (c, b'=') => Some((high | (sextet(c)? << 6), 2)),
        (c, d) => Some((high | (sextet(c)? << 6) | sextet(d)?, 3)),
    }
}

/// `DecodeString` where only valid input counts.
pub(super) fn decode_valid(input: &str) -> Option<Vec<u8>> {
    let (bytes, valid) = decode(input);
    valid.then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_go_std_encoding() {
        assert_eq!(decode_valid(""), Some(Vec::new()));
        assert_eq!(decode_valid("b3Rw"), Some(b"otp".to_vec()));
        assert_eq!(decode_valid("b3\r\nRw"), Some(b"otp".to_vec()));
        assert_eq!(decode_valid("b3Q="), Some(b"ot".to_vec()));
        assert_eq!(decode_valid("bw=="), Some(b"o".to_vec()));
        // Non-zero trailing bits are accepted, as Go's non-strict encoding does.
        assert_eq!(decode_valid("bx=="), Some(b"o".to_vec()));
        for invalid in ["b3R", "b3Q", "b===", "b3=w", "bw==b3Rw", "b3R*"] {
            assert_eq!(decode_valid(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn keeps_quanta_before_corruption_like_go() {
        assert_eq!(decode("b3Rwb3R*"), (b"otp".to_vec(), false));
        assert_eq!(decode("b3Rwbw==b3Rw"), (b"otpo".to_vec(), false));
        assert_eq!(decode("b3Rwb3"), (b"otp".to_vec(), false));
    }
}
