//! The decryptor's error type. The messages reproduce ipadecrypt's `internal/fairplay`
//! error texts.

use std::fmt::Write as _;
use std::path::PathBuf;

use crate::verify::Verification;

/// Result alias for the decryptor.
pub type Result<T> = std::result::Result<T, Error>;

/// A decryption failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A malformed IPA, Mach-O, SINF or SUPF/SUPP record, with the ported message.
    #[error("{0}")]
    Format(String),
    /// A failure decrypting one binary, with its member path.
    #[error("{path}: {source}")]
    Binary {
        /// The binary's member path in the IPA.
        path: String,
        /// What went wrong.
        #[source]
        source: Box<Error>,
    },
    /// An Apple Arcade license and no Arcade credential (`ipa.go`'s
    /// `ErrNoArcadeCredential`).
    #[error("this Apple Arcade app needs the account's Arcade credential")]
    NoArcadeCredential,
    /// The output failed verification: some binaries are still encrypted or have a
    /// zero-filled encrypted region. No output was written.
    #[error("verify failed: {}", verify_summary(.0))]
    Verify(Verification),
    /// A filesystem failure, with the operation that caused it.
    #[error("{context}: {source}")]
    Io {
        /// What was attempted.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The output path already exists; it is never overwritten.
    #[error("output already exists; never overwrite it: {0}")]
    OutputExists(PathBuf),
}

/// `decrypt.go`'s `verifyFailureSummary`, followed by the binaries it lists.
fn verify_summary(verification: &Verification) -> String {
    let mut parts = Vec::new();
    if !verification.still_encrypted.is_empty() {
        parts.push(format!(
            "{} still encrypted",
            verification.still_encrypted.len()
        ));
    }
    if !verification.all_zero_crypt.is_empty() {
        parts.push(format!(
            "{} all-zero crypt",
            verification.all_zero_crypt.len()
        ));
    }
    let mut out = parts.join(", ");
    for name in &verification.still_encrypted {
        write!(out, "; {name} still encrypted (cryptid != 0)").expect("writing to a String");
    }
    for name in &verification.all_zero_crypt {
        write!(out, "; {name} crypt region all zeros").expect("writing to a String");
    }
    out
}

/// A [`Error::Format`] from any displayable message.
pub(crate) fn format_err(message: impl Into<String>) -> Error {
    Error::Format(message.into())
}

impl Error {
    /// A [`Error::Io`] from a context label and an I/O error.
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// Go's `%q` (`strconv.Quote`) for the names in error texts.
pub(crate) fn go_quote(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            ch if u32::from(ch) < 0x20 || ch == '\x7f' => {
                write!(out, "\\x{:02x}", u32::from(ch)).expect("writing to a String");
            }
            ch if ch.is_control() => {
                write!(out, "\\u{:04x}", u32::from(ch)).expect("writing to a String");
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_quote_matches_strconv_quote() {
        assert_eq!(go_quote("priv"), "\"priv\"");
        assert_eq!(go_quote("key "), "\"key \"");
        assert_eq!(
            go_quote("a\"b\\c\n\x01\u{85}é"),
            "\"a\\\"b\\\\c\\n\\x01\\u0085é\""
        );
    }

    #[test]
    fn verify_error_lists_binaries() {
        let error = Error::Verify(Verification {
            scanned: 3,
            still_encrypted: vec!["Payload/A.app/A".into(), "Payload/A.app/B".into()],
            all_zero_crypt: vec!["Payload/A.app/C".into()],
            skipped: Vec::new(),
        });
        assert_eq!(
            error.to_string(),
            "verify failed: 2 still encrypted, 1 all-zero crypt; Payload/A.app/A still \
             encrypted (cryptid != 0); Payload/A.app/B still encrypted (cryptid != 0); \
             Payload/A.app/C crypt region all zeros"
        );
    }
}
