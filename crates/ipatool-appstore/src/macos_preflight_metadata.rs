//! Port of Go `appstore_macos_preflight_metadata.go`: the display version and build time
//! of an older macOS build, read from the small preflight package's `Distribution`.

use time::OffsetDateTime;

use crate::macos_version_metadata::xml_document;
use crate::partial_zip::VersionMetadata;
use crate::xar::{self, ReadAt};
use crate::{Error, Result};

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

/// Go's `macPreflightDistributionInfo`.
#[derive(Default)]
struct DistributionInfo {
    products: Vec<(String, String)>,
    packages: Vec<Package>,
}

struct Package {
    id: String,
    version: String,
    bundles: Vec<String>,
}

impl DistributionInfo {
    fn parse(data: &[u8]) -> Result<Self> {
        let document = xml_document(data)?;
        let root = document.root_element();
        if root.tag_name().name() != "installer-gui-script" {
            return Err(Error::Invalid(format!(
                "expected element type <installer-gui-script> but have <{}>",
                root.tag_name().name()
            )));
        }
        let attribute = |node: roxmltree::Node<'_, '_>, name| {
            node.attribute(name).unwrap_or_default().to_owned()
        };
        let mut info = Self::default();
        for child in root.children().filter(roxmltree::Node::is_element) {
            match child.tag_name().name() {
                "product" => info
                    .products
                    .push((attribute(child, "id"), attribute(child, "version"))),
                "pkg-ref" => info.packages.push(Package {
                    id: attribute(child, "id"),
                    version: attribute(child, "version"),
                    bundles: child
                        .children()
                        .filter(|node| node.has_tag_name("relocate"))
                        .flat_map(|relocate| relocate.children())
                        .filter(|node| node.has_tag_name("bundle"))
                        .map(|bundle| attribute(bundle, "id"))
                        .collect(),
                }),
                _ => {}
            }
        }
        Ok(info)
    }
}

/// Go's `macPreflightVersionMetadata`.
pub(crate) fn preflight_version_metadata<R: ReadAt>(
    archive: &xar::Reader<R>,
    bundle_id: &str,
) -> Result<VersionMetadata> {
    let mut distribution = None;
    for file in archive
        .files
        .values()
        .filter(|file| file.name == "Distribution")
    {
        if distribution.is_some() {
            return Err(invalid(
                "macOS preflight package contains multiple Distribution files",
            ));
        }
        distribution = Some(file);
    }
    let distribution = distribution
        .ok_or_else(|| invalid("macOS preflight package does not contain Distribution"))?;
    if !archive.verify_checksum(distribution) {
        return Err(invalid("macOS preflight Distribution checksum mismatch"));
    }
    let data = archive.read_file(distribution).map_err(|err| {
        let action = match err {
            xar::ReadError::Open(_) => "open",
            xar::ReadError::Read(_) => "read",
        };
        Error::Invalid(format!(
            "failed to {action} Mac preflight Distribution: {err}"
        ))
    })?;
    let info = DistributionInfo::parse(&data)
        .map_err(|err| err.context("failed to parse Mac preflight Distribution"))?;

    let [(product_id, product_version)] = info.products.as_slice() else {
        return Err(unique_product_error());
    };
    if product_id.is_empty() || (!bundle_id.is_empty() && product_id != bundle_id) {
        return Err(unique_product_error());
    }
    let version = product_version.trim();
    if version.is_empty() {
        return Err(invalid(
            "macOS preflight package does not contain a display version",
        ));
    }

    // References to the same package can be split across several pkg-ref elements. Only
    // use versions belonging to the requested product's bundle.
    let package_ids: Vec<&str> = info
        .packages
        .iter()
        .filter(|package| {
            !package.id.is_empty() && package.bundles.iter().any(|bundle| bundle == product_id)
        })
        .map(|package| package.id.as_str())
        .collect();
    let mut built: Option<OffsetDateTime> = None;
    for package in &info.packages {
        if !package_ids.contains(&package.id.as_str()) || package.version.is_empty() {
            continue;
        }
        let timestamp = legacy_package_build_time(version, &package.version)?;
        if built.is_some_and(|built| built != timestamp) {
            return Err(invalid(
                "macOS preflight package contains conflicting build timestamps",
            ));
        }
        built = Some(timestamp);
    }
    let built = built.ok_or_else(|| {
        invalid(
            "macOS preflight package does not contain a build timestamp for the requested bundle",
        )
    })?;
    // As with the payload timestamp fallback, this is the build time, not an exact store
    // release date. The API date is app-level, and the preflight archive's creation time
    // can reflect a much later repackaging date.
    Ok(VersionMetadata {
        display_version: version.to_owned(),
        release_date: built,
    })
}

fn unique_product_error() -> Error {
    invalid("macOS preflight package does not contain a unique product for the requested bundle")
}

/// Go's `legacyMacPackageBuildTime`. Legacy Apple installers use
/// `major.minor.patch.0.1.<Unix build time>`; other dotted versions are not timestamps.
fn legacy_package_build_time(
    display_version: &str,
    package_version: &str,
) -> Result<OffsetDateTime> {
    let parts: Vec<&str> = package_version.split('.').collect();
    let mut version: Vec<&str> = display_version.split('.').collect();
    while version.len() < 3 {
        version.push("0");
    }
    if parts.len() != 6
        || version.len() != 3
        || parts[..3].join(".") != version.join(".")
        || parts[3] != "0"
        || parts[4] != "1"
        || parts[5].len() != 10
    {
        return Err(invalid(
            "macOS preflight package does not contain a supported build timestamp",
        ));
    }
    if parts
        .iter()
        .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(invalid(
            "macOS preflight package contains an invalid installer version",
        ));
    }
    parts[5]
        .parse::<i64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .ok_or_else(|| invalid("macOS preflight package contains an invalid build timestamp"))
}

#[cfg(test)]
pub(crate) mod tests;
