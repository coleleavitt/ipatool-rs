//! Pinned macOS framework files run by the SAP guest. Port of Go `internal/sap/assets`.
//!
//! The cache directory, file names, sizes and SHA-256 digests match Go, so a cache filled by
//! either tool serves both.

mod remote;

pub use remote::{
    Dependency, DependencyKind, Export, ImageSection, ImageSegment, StoreCache, StoreImage,
};

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use sha2::{Digest, Sha256};

use crate::error::{Context, Error, Result};
use crate::machimage;

pub const MACOS_VERSION: &str = "27.0.1";
pub const MACOS_BUILD: &str = "26A434";
pub const MACOS_RESTORE_URL: &str = "https://updates.cdn-apple.com/2026FallFCS/59241290-5d51-4ca8-9df4-31624b9a4eac/UniversalMac_27.0.1_26A434_Restore.ipsw";
const CACHE_DIRECTORY: &str = "apple-assets-macos-27-26A434-arm64-v5";
const LEGACY_CACHE_DIRECTORY: &str = "apple-assets-macos-27-26A434-arm64-v4";

/// The frameworks, each reduced to its arm64e slice except `CoreFP.icxs`.
#[derive(Clone, Debug, Default)]
pub struct Bundle {
    pub apple_media_services: Vec<u8>,
    pub commerce: Vec<u8>,
    pub adid: Vec<u8>,
    pub core_fp: Vec<u8>,
    pub core_fp_icxs: Vec<u8>,
    pub fairplayd: Vec<u8>,
    pub fairplay_kext: Vec<u8>,
}

impl Bundle {
    fn file(&self, name: &str) -> &[u8] {
        match name {
            "AppleMediaServices" => &self.apple_media_services,
            "commerce" => &self.commerce,
            "adid" => &self.adid,
            "CoreFP" => &self.core_fp,
            "CoreFP.icxs" => &self.core_fp_icxs,
            "fairplayd" => &self.fairplayd,
            "FairPlayIOKit" => &self.fairplay_kext,
            _ => unreachable!("unknown asset {name}"),
        }
    }

    fn file_mut(&mut self, name: &str) -> &mut Vec<u8> {
        match name {
            "AppleMediaServices" => &mut self.apple_media_services,
            "commerce" => &mut self.commerce,
            "adid" => &mut self.adid,
            "CoreFP" => &mut self.core_fp,
            "CoreFP.icxs" => &mut self.core_fp_icxs,
            "fairplayd" => &mut self.fairplayd,
            "FairPlayIOKit" => &mut self.fairplay_kext,
            _ => unreachable!("unknown asset {name}"),
        }
    }
}

pub(crate) struct FileSpec {
    pub(crate) name: &'static str,
    size: usize,
    digest: &'static str,
}

pub(crate) const REQUIRED_FILES: [FileSpec; 7] = [
    FileSpec {
        name: "AppleMediaServices",
        size: 14_813_888,
        digest: "95f4558aa2c0ccd9f96ed479e336b630b46bf4b4e0baabc1c1ac8c7d55e21ab4",
    },
    FileSpec {
        name: "commerce",
        size: 2_735_088,
        digest: "c4b2088dd39aeb353f59cb57597902e8317aec66ae7da0414b03c5f932771343",
    },
    FileSpec {
        name: "adid",
        size: 3_047_808,
        digest: "43419a206c28bc93ecb8884256088d18306a0a462350f83adbced8e60a94e390",
    },
    FileSpec {
        name: "CoreFP",
        size: 30_764_976,
        digest: "afbc123e688a822d980d5816cc2b52f69fde669d1dca92cf85627e1cbee587f6",
    },
    FileSpec {
        name: "CoreFP.icxs",
        size: 7_365_344,
        digest: "cb8f55330ec567da3e692dab5f5388bed0312dd9093340477c5759078cc2aa7b",
    },
    FileSpec {
        name: "fairplayd",
        size: 17_389_744,
        digest: "1e58d37e9602dd0ca5e24b14762063b6c310248b4d3a94272771d815e4ea1784",
    },
    FileSpec {
        name: "FairPlayIOKit",
        size: 2_132_320,
        digest: "47452960ec010d3a80b02cceec73a65f8a9758850fd0fb7b2fdac7a5d1b81025",
    },
];

const LEGACY_FILES: [FileSpec; 7] = [
    FileSpec {
        name: "AppleMediaServices",
        size: 14_813_888,
        digest: "95f4558aa2c0ccd9f96ed479e336b630b46bf4b4e0baabc1c1ac8c7d55e21ab4",
    },
    FileSpec {
        name: "commerce",
        size: 8_092_640,
        digest: "76de40a4a32f691947cb926ce3b128d2fc446f7cac9755f831c406cf5a316ac9",
    },
    FileSpec {
        name: "adid",
        size: 9_797_952,
        digest: "1fec225b6917206e3d9b00fe636c04878f82bfd06a87a4b6941711de60efab5b",
    },
    FileSpec {
        name: "CoreFP",
        size: 94_842_464,
        digest: "2c30fa2b5f695fd66c0360dce72f54757d03ddb7107549715c12cb359a003b46",
    },
    FileSpec {
        name: "CoreFP.icxs",
        size: 7_365_344,
        digest: "cb8f55330ec567da3e692dab5f5388bed0312dd9093340477c5759078cc2aa7b",
    },
    FileSpec {
        name: "fairplayd",
        size: 37_689_264,
        digest: "28b4c7b2a5a0409c5020da55fc3984713a8e2b0eec8557076c8717f7166d003e",
    },
    FileSpec {
        name: "FairPlayIOKit",
        size: 2_132_320,
        digest: "47452960ec010d3a80b02cceec73a65f8a9758850fd0fb7b2fdac7a5d1b81025",
    },
];

static INSTALLED: Mutex<Option<Bundle>> = Mutex::new(None);

/// Resolves and verifies the pinned frameworks once per process: the cache first,
/// then the macOS restore image. Blocking; callers on an async runtime use `spawn_blocking`.
pub fn load() -> Result<Bundle> {
    let mut installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(bundle) = installed.as_ref() {
        return Ok(bundle.clone());
    }
    let bundle = load_bundle(&cache_directory()?)?;
    *installed = Some(bundle.clone());
    Ok(bundle)
}

fn load_bundle(directory: &Path) -> Result<Bundle> {
    if let Some(bundle) = load_cached_bundle(directory) {
        return Ok(bundle);
    }
    let bundle = compact_bundle(remote::resolve_remote_image()?)?;
    write_cache(directory, &bundle).context("cache Apple SAP assets")?;
    Ok(bundle)
}

/// Go's `os.UserCacheDir()` joined with `ipatool/sap/<version>`.
pub(crate) fn cache_directory() -> Result<PathBuf> {
    Ok(user_cache_dir()?
        .join("ipatool")
        .join("sap")
        .join(CACHE_DIRECTORY))
}

pub(crate) fn user_cache_dir() -> Result<PathBuf> {
    let missing =
        || Error::msg("find user cache directory: neither $XDG_CACHE_HOME nor $HOME are defined");
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or_else(missing)?;
        return Ok(PathBuf::from(home).join("Library/Caches"));
    }
    if cfg!(windows) {
        return std::env::var_os("LocalAppData")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| Error::msg("find user cache directory: %LocalAppData% is not defined"));
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|dir| !dir.is_empty()) {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            return Err(Error::msg(
                "find user cache directory: path in $XDG_CACHE_HOME is relative",
            ));
        }
        return Ok(dir);
    }
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(missing)?;
    Ok(PathBuf::from(home).join(".cache"))
}

/// Keeps only the architecture the emulator runs, after the original files were verified.
fn compact_bundle(mut bundle: Bundle) -> Result<Bundle> {
    for name in [
        "AppleMediaServices",
        "commerce",
        "adid",
        "CoreFP",
        "fairplayd",
        "FairPlayIOKit",
    ] {
        let data = bundle.file_mut(name);
        let slice =
            machimage::arm64_slice(data).with_context(|| format!("select {name} architecture"))?;
        if slice.len() != data.len() {
            *data = slice.to_vec();
        }
    }
    Ok(bundle)
}

fn load_cached_bundle(directory: &Path) -> Option<Bundle> {
    if let Ok(bundle) = read_cache(directory, &REQUIRED_FILES) {
        return Some(bundle);
    }
    let legacy = directory.parent()?.join(LEGACY_CACHE_DIRECTORY);
    let bundle = read_cache(&legacy, &LEGACY_FILES).ok()?;
    validate(&bundle).ok()?;
    // A verified legacy cache stays usable on read-only mounts, so a failed rewrite only
    // means the next run reads the legacy directory again.
    write_cache(directory, &bundle).ok();
    Some(bundle)
}

fn read_cache(directory: &Path, specs: &[FileSpec]) -> Result<Bundle> {
    let mut bundle = Bundle::default();
    for spec in specs {
        *bundle.file_mut(spec.name) = read_cache_file(&directory.join(spec.name), spec)
            .with_context(|| format!("read cached Apple SAP asset {}", spec.name))?;
    }
    Ok(bundle)
}

struct HashingReader<'a, R> {
    inner: R,
    digest: &'a mut Sha256,
}

impl<R: Read> Read for HashingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buf)?;
        self.digest.update(&buf[..count]);
        Ok(count)
    }
}

fn read_cache_file(path: &Path, spec: &FileSpec) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path).context("open cached asset")?;
    let size = file.metadata().context("inspect cached asset")?.len();
    if size != spec.size as u64 {
        return Err(Error::msg(format!(
            "has size {size}, expected {}",
            spec.size
        )));
    }
    let mut digest = Sha256::new();
    let mut reader = HashingReader {
        inner: &mut file,
        digest: &mut digest,
    };
    let data = if spec.name == "CoreFP.icxs" {
        let mut data = vec![0; spec.size];
        reader
            .read_exact(&mut data)
            .map(|()| data)
            .map_err(Error::from)
    } else {
        machimage::read_arm64(&mut reader, spec.size)
    }
    .context("read cached asset")?;
    let mut extra = [0u8; 1];
    if file
        .read(&mut extra)
        .context("verify cached asset length")?
        != 0
    {
        return Err(Error::msg(format!(
            "grew beyond expected size {} while being read",
            spec.size
        )));
    }
    if hex(&digest.finalize()) != spec.digest {
        return Err(Error::msg(format!(
            "apple SAP asset {} failed integrity verification",
            spec.name
        )));
    }
    Ok(data)
}

fn write_cache(directory: &Path, bundle: &Bundle) -> Result<()> {
    validate(bundle)?;
    create_private_dir(directory).context("create SAP asset cache")?;
    for spec in &REQUIRED_FILES {
        replace_file(&directory.join(spec.name), bundle.file(spec.name))
            .with_context(|| format!("write cached SAP asset {}", spec.name))?;
    }
    Ok(())
}

fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(directory)
}

fn replace_file(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::Builder::new()
        .prefix(".download-")
        .tempfile_in(parent)
        .context("create temporary cache file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("set cache file permissions")?;
    }
    std::io::Write::write_all(&mut temporary.as_file(), data)
        .context("write temporary cache file")?;
    if let Err(error) = temporary.persist(path) {
        if path.exists() {
            fs::remove_file(path).context("remove existing cache file")?;
        }
        error
            .file
            .persist(path)
            .map_err(|error| error.error)
            .context("install cache file")?;
    }
    Ok(())
}

fn validate(bundle: &Bundle) -> Result<()> {
    REQUIRED_FILES
        .iter()
        .try_for_each(|spec| validate_file(spec, bundle.file(spec.name)))
}

pub(crate) fn validate_file(spec: &FileSpec, data: &[u8]) -> Result<()> {
    if data.len() != spec.size {
        return Err(Error::msg(format!(
            "apple SAP asset {} has size {}, expected {}",
            spec.name,
            data.len(),
            spec.size
        )));
    }
    if hex(&Sha256::digest(data)) != spec.digest {
        return Err(Error::msg(format!(
            "apple SAP asset {} failed integrity verification",
            spec.name
        )));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[usize::from(byte >> 4)],
                DIGITS[usize::from(byte & 0xf)],
            ]
        })
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the cache Go filled. Opt-in: `IPATOOL_TEST_SAP_CACHE=1`.
    #[test]
    fn reads_go_cache() {
        if std::env::var_os("IPATOOL_TEST_SAP_CACHE").is_none() {
            return;
        }
        let bundle = read_cache(&cache_directory().unwrap(), &REQUIRED_FILES).unwrap();
        validate(&bundle).unwrap();
        assert_eq!(&bundle.core_fp[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    }

    #[test]
    fn rejects_tampered_cache() {
        let dir = tempfile::tempdir().unwrap();
        let spec = FileSpec {
            name: "commerce",
            size: 4,
            digest: "00",
        };
        fs::write(dir.path().join("commerce"), b"abcd").unwrap();
        let error = read_cache_file(&dir.path().join("commerce"), &spec).unwrap_err();
        assert_eq!(
            error.to_string(),
            "apple SAP asset commerce failed integrity verification"
        );
        fs::write(dir.path().join("commerce"), b"abc").unwrap();
        let error = read_cache_file(&dir.path().join("commerce"), &spec).unwrap_err();
        assert_eq!(error.to_string(), "has size 3, expected 4");
    }
}
