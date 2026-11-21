//! The IPA-level decryption pipeline: find each encrypted arm64 binary with its SINF and
//! SUPF records, decrypt it, and write a new IPA with the decrypted binaries in place.
//!
//! A port of `decrypt.py` from EthanArbuckle/fairplay-decrypt (GPL-3). Its behavior and
//! error texts are reproduced.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use ipatool_appstore::IpaReader;

use crate::core::{arm64_encryption_infos, decrypt_binary};
use crate::error::{Error, Result, format_err};

/// What one decryption run produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The output IPA path.
    pub output: PathBuf,
    /// The number of binaries that were decrypted.
    pub decrypted_binaries: usize,
    /// The total number of decrypted pages.
    pub pages: usize,
}

/// One decryptable binary and the records that decrypt it.
struct PlanEntry {
    binary_path: String,
    source: Vec<u8>,
    sinf: Vec<u8>,
    supf: Vec<u8>,
}

/// `decrypt.py`'s `supf_paths`: the SUPF member for each binary, v5 preferred, sorted.
fn supf_paths(names: &[String]) -> Vec<String> {
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort();
    let mut by_binary: BTreeMap<String, String> = BTreeMap::new();
    for name in sorted {
        if let Some(stem) = name.strip_suffix(".v5.supf") {
            by_binary.insert(stem.to_owned(), name.clone());
        } else if let Some(stem) = name.strip_suffix(".supf") {
            by_binary
                .entry(stem.to_owned())
                .or_insert_with(|| name.clone());
        }
    }
    let mut paths: Vec<String> = by_binary.into_values().collect();
    paths.sort();
    paths
}

/// Python's `str.count("/")`.
fn count_slashes(value: &str) -> usize {
    value.bytes().filter(|&byte| byte == b'/').count()
}

/// Python's `pathlib.Path(prefix).stem`: the final path component without its last suffix.
fn path_stem(prefix: &str) -> String {
    let base = prefix.rsplit('/').next().unwrap_or(prefix);
    match base.rfind('.') {
        // A leading dot is not a suffix separator (`.foo` has stem `.foo`).
        Some(0) | None => base.to_owned(),
        Some(dot) => base[..dot].to_owned(),
    }
}

/// `decrypt.py`'s `enclosing_app_sinf`: the app's own SINF, used when a binary has none.
// The suffix checks mirror Python's case-sensitive `str.endswith`, so they must stay literal.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "matches decrypt.py's case-sensitive str.endswith"
)]
fn enclosing_app_sinf(names: &HashSet<String>, bundle: &str) -> Option<String> {
    let parts: Vec<&str> = bundle.split('/').collect();
    for i in (1..=parts.len()).rev() {
        let prefix = parts[..i].join("/");
        if !prefix.ends_with(".app") {
            continue;
        }
        let sc_info = format!("{prefix}/SC_Info/");
        let depth = count_slashes(&sc_info);
        let mut sinfs: Vec<&String> = names
            .iter()
            .filter(|name| {
                name.starts_with(&sc_info)
                    && name.ends_with(".sinf")
                    && count_slashes(name) == depth
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
        if let Some(name) = sinfs.iter().find(|name| !name.ends_with(".v4.sinf")) {
            return Some((*name).clone());
        }
        return Some(sinfs[0].clone());
    }
    None
}

/// `decrypt.py`'s plan: for each SUPF, the binary and the SINF that decrypt it.
fn build_plan(
    reader: &IpaReader,
    names: &HashSet<String>,
    members: &[String],
) -> Result<Vec<PlanEntry>> {
    let mut plan = Vec::new();
    for supf_path in supf_paths(members) {
        let (bundle, filename) = supf_path
            .rsplit_once("/SC_Info/")
            .ok_or_else(|| format_err(format!("SUPF outside SC_Info: {supf_path}")))?;
        let is_v5 = filename.ends_with(".v5.supf");
        let stem = if is_v5 {
            filename.strip_suffix(".v5.supf").expect("checked suffix")
        } else {
            filename.strip_suffix(".supf").unwrap_or(filename)
        };
        let binary_path = format!("{bundle}/{stem}");
        let mut sinf_path = if is_v5 {
            format!("{bundle}/SC_Info/{stem}.v4.sinf")
        } else {
            format!("{bundle}/SC_Info/{stem}.sinf")
        };
        if !names.contains(&sinf_path) && is_v5 {
            sinf_path = format!("{bundle}/SC_Info/{stem}.sinf");
        }
        if !names.contains(&binary_path) {
            return Err(format_err(format!("SUPF lacks binary/SINF: {supf_path}")));
        }
        let source = read_member(reader, &binary_path)?;
        if arm64_encryption_infos(&source)?.is_empty() {
            continue;
        }
        if !names.contains(&sinf_path) {
            sinf_path = enclosing_app_sinf(names, bundle)
                .ok_or_else(|| format_err(format!("SUPF lacks binary/SINF: {supf_path}")))?;
        }
        let sinf = read_member(reader, &sinf_path)?;
        let supf = read_member(reader, &supf_path)?;
        plan.push(PlanEntry {
            binary_path,
            source,
            sinf,
            supf,
        });
    }
    if plan.is_empty() {
        return Err(format_err(
            "no encrypted arm64 binaries with supported SINF/SUPF",
        ));
    }
    Ok(plan)
}

fn read_member(reader: &IpaReader, name: &str) -> Result<Vec<u8>> {
    reader
        .read(name)
        .map_err(|error| Error::io(format!("failed to read {name}"), error))?
        .ok_or_else(|| format_err(format!("member missing: {name}")))
}

/// Decrypts the IPA at `input` with `credential` (16 bytes) and writes the result to
/// `output`, which must not already exist. A port of `decrypt.py`'s `main`.
///
/// # Errors
/// If the credential is not 16 bytes, the output exists, the IPA has no decryptable
/// binary, or any record is malformed.
pub fn decrypt_ipa(input: &Path, output: &Path, credential: &[u8]) -> Result<Summary> {
    if credential.len() != 16 {
        return Err(format_err("credential must contain exactly 16 bytes"));
    }
    if output.exists() {
        return Err(Error::OutputExists(output.to_path_buf()));
    }
    let reader =
        IpaReader::open(input).map_err(|error| Error::io("failed to open input IPA", error))?;
    let members: Vec<String> = reader.names().into_iter().map(str::to_owned).collect();
    let names: HashSet<String> = members.iter().cloned().collect();
    if names.len() != members.len() {
        return Err(format_err("input ZIP has duplicate member names"));
    }
    let plan = build_plan(&reader, &names, &members)?;

    let mut replacements: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut total_pages = 0;
    for entry in &plan {
        let (result, pages) = decrypt_binary(&entry.source, &entry.sinf, &entry.supf, credential)?;
        total_pages += pages;
        replacements.insert(entry.binary_path.clone(), result);
    }
    let decrypted_binaries = replacements.len();

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| Error::io("failed to create output directory", error))?;
    }
    write_output(&reader, output, &replacements)?;
    Ok(Summary {
        output: output.to_path_buf(),
        decrypted_binaries,
        pages: total_pages,
    })
}

/// Writes the output IPA to a temporary file beside `output`, then renames it into place,
/// as `decrypt.py` writes a provisional file and links it to the final path.
fn write_output(
    reader: &IpaReader,
    output: &Path,
    replacements: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let parent = output.parent().filter(|path| !path.as_os_str().is_empty());
    let mut temp = match parent {
        Some(dir) => tempfile::Builder::new()
            .prefix("fairplay-output-")
            .tempfile_in(dir),
        None => tempfile::Builder::new()
            .prefix("fairplay-output-")
            .tempfile(),
    }
    .map_err(|error| Error::io("failed to create temporary output", error))?;
    reader
        .rewrite(temp.as_file_mut(), replacements)
        .map_err(|error| Error::io("failed to write output IPA", error))?;
    temp.persist_noclobber(output)
        .map_err(|error| Error::io("failed to finalize output IPA", error.error))?;
    Ok(())
}
