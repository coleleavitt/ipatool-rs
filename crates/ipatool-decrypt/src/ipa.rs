//! The IPA-level decryption pipeline: find each encrypted binary with its SINF and
//! SUPF/SUPP records, decrypt it with the credential its license needs, write a new IPA
//! with the decrypted binaries in place, and verify the result.
//!
//! A port of ipadecrypt's `internal/fairplay/ipa.go` and the parts of
//! `internal/pipeline/assemble.go` it uses (MIT), which follow `decrypt.py` from
//! EthanArbuckle/fairplay-decrypt (GPL-3). Its behavior and error texts are reproduced.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use ipatool_appstore::IpaReader;

use crate::core::{CREDENTIAL_SIZE, arm_encryption_info, decrypt_binary, is_arcade_sinf};
use crate::error::{Error, Result, format_err, go_quote};
use crate::verify::verify_members;

/// One account's `FairPlay` credentials: `standard` decodes regular App Store licenses,
/// `arcade` Apple Arcade ones (`ipa.go`'s `Credentials`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    /// The 16-byte credential for regular licenses.
    pub standard: Vec<u8>,
    /// The account's 16-byte Arcade credential, when it has one.
    pub arcade: Option<Vec<u8>>,
}

/// Which parts of the app to decrypt and keep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Leave `.appex` binaries out of the plan; they stay encrypted in the output.
    pub skip_appex: bool,
    /// Keep and decrypt the Watch app instead of dropping it from the output.
    pub keep_watch: bool,
}

/// What one decryption run produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The output IPA path.
    pub output: PathBuf,
    /// The number of binaries that were decrypted.
    pub binaries: usize,
    /// The total number of decrypted pages.
    pub pages: usize,
    /// The number of Mach-Os the verification pass checked in the output.
    pub verified: usize,
}

#[derive(Debug)]
struct PlanEntry {
    binary_path: String,
    supf_path: String,
    sinf_path: String,
    bundle: String,
}

/// `ipa.go`'s `pageKeySuffixes`, best first: newer generations ahead of legacy ones, SUPF
/// (f-class) ahead of SUPP (p-class) within each. An iOS-26-only binary ships a 0-byte
/// `.supf` and no `.supp` next to its `.v5.supf`/`.v4.supp`, which therefore win.
const PAGE_KEY_SUFFIXES: [&str; 4] = [".v5.supf", ".v4.supp", ".supf", ".supp"];

/// `ipa.go`'s `pageKeySuffix`: the name's rank and suffix in [`PAGE_KEY_SUFFIXES`].
fn page_key_suffix(name: &str) -> Option<(usize, &'static str)> {
    PAGE_KEY_SUFFIXES
        .iter()
        .enumerate()
        .find(|(_, suffix)| name.ends_with(*suffix))
        .map(|(rank, suffix)| (rank, *suffix))
}

/// `ipa.go`'s `supfPaths`: per binary stem, the SUPF/SUPP whose suffix ranks best, sorted.
fn supf_paths(names: &HashSet<String>) -> Vec<String> {
    let mut best: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
    for name in names {
        let Some((rank, suffix)) = page_key_suffix(name) else {
            continue;
        };
        let stem = &name[..name.len() - suffix.len()];
        let current = best.entry(stem).or_insert((rank, name));
        if rank < current.0 {
            *current = (rank, name);
        }
    }
    let mut paths: Vec<String> = best
        .into_values()
        .map(|(_, name)| name.to_owned())
        .collect();
    paths.sort();
    paths
}

/// `ipa.go`'s `planEntries`: every SUPF/SUPP mapped to its binary and SINF.
fn plan_entries(names: &HashSet<String>) -> Result<Vec<PlanEntry>> {
    let mut entries = Vec::new();
    for supf_path in supf_paths(names) {
        let Some((bundle, filename)) = supf_path.rsplit_once("/SC_Info/") else {
            return Err(format_err(format!(
                "SUPF/SUPP outside SC_Info: {supf_path}"
            )));
        };
        let suffix = page_key_suffix(filename).map_or("", |(_, suffix)| suffix);
        let stem = filename.strip_suffix(suffix).unwrap_or(filename);
        let binary_path = format!("{bundle}/{stem}");
        let mut sinf_path = format!("{bundle}/SC_Info/{stem}.sinf");
        if suffix == ".v5.supf" || suffix == ".v4.supp" {
            let v4 = format!("{bundle}/SC_Info/{stem}.v4.sinf");
            if names.contains(&v4) {
                sinf_path = v4;
            }
        }
        if !names.contains(&binary_path) {
            return Err(format_err(format!(
                "SUPF/SUPP lacks binary/SINF: {supf_path}"
            )));
        }
        entries.push(PlanEntry {
            binary_path,
            sinf_path,
            bundle: bundle.to_owned(),
            supf_path,
        });
    }
    Ok(entries)
}

/// Python's `pathlib.Path(prefix).stem` / Go's `TrimSuffix(base, path.Ext(base))`.
fn path_stem(prefix: &str) -> &str {
    let base = prefix.rsplit('/').next().unwrap_or(prefix);
    base.rfind('.').map_or(base, |dot| &base[..dot])
}

/// `ipa.go`'s `enclosingAppSinf`: a SINF of the nearest enclosing `.app`, used when an
/// extension or framework has none of its own.
// The suffix checks mirror Go's case-sensitive `strings.HasSuffix`, so they must stay literal.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "matches ipa.go's case-sensitive strings.HasSuffix"
)]
fn enclosing_app_sinf(names: &HashSet<String>, bundle: &str) -> Option<String> {
    let parts: Vec<&str> = bundle.split('/').collect();
    for i in (1..=parts.len()).rev() {
        let prefix = parts[..i].join("/");
        if !prefix.ends_with(".app") {
            continue;
        }
        let sc_info = format!("{prefix}/SC_Info/");
        let depth = sc_info.matches('/').count();
        let mut sinfs: Vec<&String> = names
            .iter()
            .filter(|name| {
                name.starts_with(&sc_info)
                    && name.ends_with(".sinf")
                    && name.matches('/').count() == depth
            })
            .collect();
        if sinfs.is_empty() {
            continue;
        }
        sinfs.sort();
        let stem = path_stem(&prefix);
        for suffix in [".sinf", ".v4.sinf"] {
            let preferred = format!("{sc_info}{stem}{suffix}");
            if sinfs.iter().any(|name| **name == preferred) {
                return Some(preferred);
            }
        }
        let fallback = sinfs
            .iter()
            .find(|name| !name.ends_with(".v4.sinf"))
            .unwrap_or(&sinfs[0]);
        return Some((*fallback).clone());
    }
    None
}

/// `assemble.go`'s `isExcludedIPAEntry`: the members the on-device packager leaves out
/// (`SC_Info`, `Watch`/`WatchKitSupport2` unless `keep_watch`, dSYMs, `BCSymbolMaps`,
/// `Symbols`, `META-INF`, `iTunesMetadata.plist`, `iTunesArtwork`).
fn is_excluded_ipa_entry(name: &str, keep_watch: bool) -> bool {
    let name = name.strip_suffix('/').unwrap_or(name);
    if matches!(
        name,
        "iTunesMetadata.plist" | "Payload/iTunesMetadata.plist" | "Payload/iTunesArtwork"
    ) || name == "Payload/META-INF"
        || name.starts_with("Payload/META-INF/")
    {
        return true;
    }
    // Segments inside Payload/<app>.app/.
    name.split('/').skip(2).any(|segment| match segment {
        "Watch" | "WatchKitSupport2" => !keep_watch,
        "SC_Info" | "BCSymbolMaps" | "Symbols" => true,
        _ => segment.ends_with(".dSYM"),
    })
}

fn read_member(reader: &IpaReader, name: &str) -> Result<Vec<u8>> {
    reader
        .read(name)
        .map_err(|error| Error::io(format!("read {name}"), error))?
        .ok_or_else(|| format_err("missing zip member"))
}

fn in_binary(path: &str) -> impl FnOnce(Error) -> Error + '_ {
    move |error| Error::Binary {
        path: path.to_owned(),
        source: Box::new(error),
    }
}

/// Decrypts every binary in the plan; the decrypted thin Mach-Os by member name and the
/// page count.
fn decrypt_plan(
    reader: &IpaReader,
    names: &HashSet<String>,
    credentials: &Credentials,
    options: Options,
) -> Result<(BTreeMap<String, Vec<u8>>, usize)> {
    let mut replacements = BTreeMap::new();
    let mut total_pages = 0;
    for entry in plan_entries(names)? {
        if options.skip_appex && entry.binary_path.contains(".appex/") {
            continue;
        }
        // The output drops Watch apps unless keep_watch.
        if !options.keep_watch && entry.binary_path.contains("/Watch/") {
            continue;
        }
        let source = read_member(reader, &entry.binary_path)?;
        if arm_encryption_info(&source)
            .map_err(in_binary(&entry.binary_path))?
            .is_none()
        {
            continue;
        }
        let sinf_path = if names.contains(&entry.sinf_path) {
            entry.sinf_path
        } else {
            enclosing_app_sinf(names, &entry.bundle).ok_or_else(|| {
                format_err(format!("SUPF/SUPP lacks binary/SINF: {}", entry.supf_path))
            })?
        };
        let sinf = read_member(reader, &sinf_path)?;
        let supf = read_member(reader, &entry.supf_path)?;
        let credential = if is_arcade_sinf(&sinf) {
            credentials
                .arcade
                .as_deref()
                .ok_or(Error::NoArcadeCredential)?
        } else {
            &credentials.standard
        };
        let Some((result, pages)) = decrypt_binary(&source, &sinf, &supf, credential)
            .map_err(in_binary(&entry.binary_path))?
        else {
            continue;
        };
        replacements.insert(entry.binary_path, result);
        total_pages += pages;
    }
    if replacements.is_empty() {
        return Err(format_err(
            "no encrypted binaries with supported SINF/SUPF/SUPP",
        ));
    }
    Ok((replacements, total_pages))
}

/// Decrypts the IPA at `input` with the account's `credentials` and writes the result to
/// `output`, which must not already exist (`ipa.go`'s `Decrypt` plus the verification
/// pass of ipadecrypt's offline path). Fat binaries come out thinned to one slice;
/// `SC_Info`, `iTunesMetadata` and the other members the device packager excludes are
/// dropped. The output is verified before it is moved into place: a binary left with
/// `cryptid` != 0 or a zero-filled encrypted region fails with [`Error::Verify`] and no
/// output is written.
///
/// # Errors
/// If a credential is not 16 bytes, the output exists, the IPA has no decryptable binary,
/// an Arcade license has no Arcade credential, a record is malformed, or the output fails
/// verification.
pub fn decrypt_ipa(
    input: &Path,
    output: &Path,
    credentials: &Credentials,
    options: Options,
) -> Result<Summary> {
    if credentials.standard.len() != CREDENTIAL_SIZE
        || credentials
            .arcade
            .as_ref()
            .is_some_and(|arcade| arcade.len() != CREDENTIAL_SIZE)
    {
        return Err(format_err("credential must contain exactly 16 bytes"));
    }
    if output.exists() {
        return Err(Error::OutputExists(output.to_path_buf()));
    }
    let reader = IpaReader::open(input)
        .map_err(|error| Error::io(format!("open ipa {}", input.display()), error))?;
    let mut names = HashSet::new();
    for name in reader.names() {
        if !names.insert(name.to_owned()) {
            return Err(format_err(format!(
                "input zip has duplicate member {}",
                go_quote(name)
            )));
        }
    }
    let (replacements, pages) = decrypt_plan(&reader, &names, credentials, options)?;
    let binaries = replacements.len();

    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|error| Error::io("failed to create output directory", error))?;
    }
    let verified = write_output(&reader, output, &replacements, options)?;
    Ok(Summary {
        output: output.to_path_buf(),
        binaries,
        pages,
        verified,
    })
}

/// Writes the output IPA to a temporary file beside `output`, verifies it, then moves it
/// into place. Returns the number of Mach-Os verified.
fn write_output(
    reader: &IpaReader,
    output: &Path,
    replacements: &BTreeMap<String, Vec<u8>>,
    options: Options,
) -> Result<usize> {
    let parent = output.parent().filter(|path| !path.as_os_str().is_empty());
    let mut temp = match parent {
        Some(dir) => tempfile::Builder::new()
            .prefix("fairplay-output-")
            .tempfile_in(dir),
        None => tempfile::Builder::new()
            .prefix("fairplay-output-")
            .tempfile_in("."),
    }
    .map_err(|error| Error::io("failed to create temporary output", error))?;
    reader
        .rewrite(temp.as_file_mut(), replacements, |name| {
            !is_excluded_ipa_entry(name, options.keep_watch)
        })
        .map_err(|error| Error::io("failed to write output IPA", error))?;

    let written = IpaReader::open(temp.path())
        .map_err(|error| Error::io("failed to reopen output IPA", error))?;
    let verification = verify_members(
        written.members().map(|(name, data)| {
            data.map(|data| (name.to_owned(), data))
                .map_err(|error| Error::io(format!("read {name}"), error))
        }),
        options.skip_appex,
    )?;
    if !verification.ok() {
        return Err(Error::Verify(verification));
    }
    temp.persist_noclobber(output)
        .map_err(|error| Error::io("failed to finalize output IPA", error.error))?;
    Ok(verification.scanned)
}

#[cfg(test)]
mod tests;
