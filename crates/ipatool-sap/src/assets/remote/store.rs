//! Port of Go `assets/store.go` and `assets/store_images.go`: the pinned shared cache read
//! for the Store profile without loading any host framework.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use reqwest::blocking::Client;

use super::{
    ARM64_CACHE_PATH, Cursor, FILE_SYSTEM_ARCHIVE_SIZE, FILE_SYSTEM_IMAGE_NAME,
    FILE_SYSTEM_IMAGE_SIZE, ReadAt, SYSTEM_OS_ARCHIVE_SIZE, SYSTEM_OS_IMAGE_NAME,
    SYSTEM_OS_IMAGE_SIZE, Section, aea, apfs, dyld, http_range, macho, pinned_zip_entry,
};
use crate::assets::MACOS_RESTORE_URL;
use crate::error::{Context, Error, Result};

const AUTHENTICATION_PLUGIN_IMAGE: &str = "/System/Library/Accounts/Authentication/AMSAccountAuthenticationPlugin.bundle/Contents/MacOS/AMSAccountAuthenticationPlugin";
const MAXIMUM_SYSTEM_FILE_SIZE: u64 = 4 << 20;

type SystemFileReader<'a> = Box<dyn Fn(&str) -> Result<Vec<u8>> + 'a>;

/// Go's `StoreCache`: the shared cache used by the profile generator, opened from local
/// copies or directly from Apple's restore image.
pub struct StoreCache<'a> {
    cache: dyld::Cache<'a>,
    slides: RefCell<HashMap<u64, HashMap<u64, u64>>>,
    images: OnceLock<HashMap<String, u64>>,
    read_system_file: SystemFileReader<'a>,
}

/// A shared-cache image's Mach-O metadata, the subset of go-macho's `File` the profile
/// generator reads.
pub struct StoreImage {
    image: dyld::Image,
    base_address: u64,
    exports_trie: Option<(u32, u32)>,
    pub uuid: String,
    pub sections: Vec<ImageSection>,
    pub segments: Vec<ImageSegment>,
    pub dependencies: Vec<Dependency>,
}

pub struct ImageSection {
    pub name: String,
    pub segment: String,
    pub address: u64,
    pub size: u64,
    pub flags: u32,
}

impl ImageSection {
    /// go-macho `SectionFlag.IsPureInstructions() || IsSomeInstructions()`.
    #[must_use]
    pub fn has_instructions(&self) -> bool {
        self.flags & 0x8000_0000 != 0 || self.flags & 0x400 != 0
    }
}

pub struct ImageSegment {
    pub name: String,
    pub address: u64,
    pub memory_size: u64,
    pub offset: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyKind {
    Load,
    Weak,
    ReExport,
    Upward,
    LazyLoad,
}

/// A dependent-library load command; `flags` are the dylib-use flags (0 when absent).
pub struct Dependency {
    pub kind: DependencyKind,
    pub name: String,
    pub flags: u32,
}

/// One go-macho `trie.TrieExport`, without the re-export target.
pub struct Export {
    pub name: String,
    pub flags: u64,
    pub address: u64,
}

impl Export {
    /// go-macho `ExportFlag.ReExport`, which compares the whole flag value.
    #[must_use]
    pub fn is_reexport(&self) -> bool {
        self.flags == 8
    }
}

impl StoreImage {
    /// go-macho `File.GetBaseAddress`: the `__TEXT` address.
    #[must_use]
    pub fn base_address(&self) -> u64 {
        self.base_address
    }

    #[must_use]
    pub fn has_exports_trie(&self) -> bool {
        self.exports_trie.is_some()
    }

    #[must_use]
    pub fn segment(&self, name: &str) -> Option<&ImageSegment> {
        self.segments.iter().find(|segment| segment.name == name)
    }
}

impl StoreCache<'static> {
    /// Go `OpenLocalStoreCache`: shared-cache files at `path` plus member suffixes, with
    /// feature defaults read from the host's `/System`.
    pub fn open_local(path: &Path) -> Result<Self> {
        let path = path.as_os_str().to_owned();
        let opener = move |suffix: &str| -> Result<dyld::MemberFile<'static>> {
            let mut name = path.clone();
            name.push(suffix);
            let file = std::fs::File::open(&name).context("open Store cache member")?;
            let size = file.metadata().context("inspect Store cache member")?.len();
            Ok(dyld::MemberFile {
                reader: Box::new(LocalFile(file)),
                size,
            })
        };
        Self::new(&opener, Box::new(|path| Ok(std::fs::read(path)?)))
    }
}

impl<'a> StoreCache<'a> {
    /// Go `OpenRemoteStoreCache`: reads the pinned restore image over HTTP ranges. The
    /// readers borrow each other, so the cache lives for the duration of `run`.
    pub fn with_remote<T>(run: impl FnOnce(&StoreCache<'_>) -> Result<T>) -> Result<T> {
        let client = Client::new();
        let remote = http_range::RemoteFile::open(client.clone(), MACOS_RESTORE_URL)
            .context("open Store restore image")?;
        let mut archive = zip::ZipArchive::new(Cursor {
            source: &remote,
            size: remote.size(),
            position: 0,
        })
        .context("read Store restore archive")?;
        let entry = pinned_zip_entry(
            &mut archive,
            SYSTEM_OS_IMAGE_NAME,
            SYSTEM_OS_ARCHIVE_SIZE,
            SYSTEM_OS_ARCHIVE_SIZE,
        )?;
        let encrypted = Section::new(&remote, entry.data_offset, entry.compressed_size);
        let image = aea::AeaReader::open(&client, &encrypted, entry.compressed_size)
            .context("open Store cryptex")?;
        if image.size() != SYSTEM_OS_IMAGE_SIZE {
            return Err(Error::msg("store cryptex does not match the pinned image"));
        }
        let extractor = apfs::Extractor::open(&image, image.size())?;
        let opener = |suffix: &str| -> Result<dyld::MemberFile<'_>> {
            extractor
                .open_file(&format!("{ARM64_CACHE_PATH}{suffix}"))
                .map(Into::into)
                .context("open Store cache member")
        };

        let system_entry = pinned_zip_entry(
            &mut archive,
            FILE_SYSTEM_IMAGE_NAME,
            FILE_SYSTEM_ARCHIVE_SIZE,
            FILE_SYSTEM_ARCHIVE_SIZE,
        )?;
        let system_encrypted = Section::new(
            &remote,
            system_entry.data_offset,
            system_entry.compressed_size,
        );
        // Like Go, the filesystem image is opened on the first system-file read.
        let system_image = OnceCell::new();
        let system = OnceCell::new();
        let read_system_file = |path: &str| -> Result<Vec<u8>> {
            let extractor = if let Some(extractor) = system.get() {
                extractor
            } else {
                let image =
                    aea::AeaReader::open(&client, &system_encrypted, system_entry.compressed_size)?;
                if image.size() != FILE_SYSTEM_IMAGE_SIZE {
                    return Err(Error::msg("store filesystem does not match pinned image"));
                }
                let image = system_image.get_or_init(|| image);
                let extractor = apfs::Extractor::open(image, image.size())?;
                system.get_or_init(|| extractor)
            };
            read_system_file(extractor, path)
        };
        let cache = StoreCache::new(&opener, Box::new(read_system_file))?;
        run(&cache)
    }

    fn new(opener: &dyld::Opener<'_, 'a>, read_system_file: SystemFileReader<'a>) -> Result<Self> {
        let cache = dyld::Cache::open(opener).context("open Store shared cache")?;
        Ok(Self {
            cache,
            slides: RefCell::new(HashMap::new()),
            images: OnceLock::new(),
            read_system_file,
        })
    }

    /// Go `StoreCache.Page`.
    pub fn page(&self, address: u64) -> Result<Vec<u8>> {
        self.cache.page(address, &mut self.slides.borrow_mut())
    }

    /// Go `StoreCache.ExtractAuthenticationPlugin`, returning the exported image.
    pub fn extract_authentication_plugin(&self) -> Result<Vec<u8>> {
        self.cache.extract_image(AUTHENTICATION_PLUGIN_IMAGE)
    }

    /// Go `StoreCache.FeatureFlags`: the pinned operating-system feature defaults.
    pub fn feature_flags(&self, domain: &str) -> Result<Vec<u8>> {
        if !matches!(domain, "AppleMediaServices" | "Foundation" | "CFNetwork") {
            return Err(Error::msg("unsupported Store feature domain"));
        }
        (self.read_system_file)(&format!(
            "/System/Library/FeatureFlags/Domain/{domain}.plist"
        ))
    }

    /// Go `StoreCache.AddressRange`.
    #[must_use]
    pub fn address_range(&self) -> (u64, u64) {
        self.cache.address_range()
    }

    /// Go `StoreCache.Contains`.
    #[must_use]
    pub fn contains(&self, address: u64, size: u64) -> bool {
        self.cache.contains(address, size)
    }

    /// Go `StoreCache.Images`.
    pub fn images(&self) -> Result<&HashMap<String, u64>> {
        if let Some(images) = self.images.get() {
            return Ok(images);
        }
        let images = self.cache.image_addresses()?;
        Ok(self.images.get_or_init(|| images))
    }

    /// Go `StoreCache.OpenImage`: the load commands of one image, without its symbols.
    pub fn open_image(&self, name: &str) -> Result<StoreImage> {
        let address = *self
            .images()?
            .get(name)
            .ok_or_else(|| Error::msg(format!("image {name} was not found")))?;
        let image = self.cache.image_at(address)?;
        let file = self
            .cache
            .open_image(&image)
            .context("parse shared-cache image")?;
        Ok(store_image(image, &file))
    }

    /// go-macho `File.DyldExports` for a shared-cache image.
    pub fn dyld_exports(&self, image: &StoreImage) -> Result<Vec<Export>> {
        let Some((offset, size)) = image.exports_trie else {
            return Err(Error::msg("macho does not contain LC_DYLD_EXPORTS_TRIE"));
        };
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut data = vec![0u8; size as usize];
        self.cache
            .read_linkedit(&image.image, &mut data, u64::from(offset))
            .with_context(|| {
                format!("failed to read LC_DYLD_EXPORTS_TRIE data at offset={offset:#x}")
            })?;
        Ok(macho::parse_trie_exports(&data, image.base_address)
            .context("failed to parse LC_DYLD_EXPORTS_TRIE")?
            .into_iter()
            .map(|export| Export {
                name: export.name,
                flags: export.flags,
                address: export.address,
            })
            .collect())
    }
}

fn store_image(image: dyld::Image, file: &macho::File) -> StoreImage {
    let uuid = file.uuid().map_or_else(String::new, |u| {
        format!(
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], u[8], u[9], u[10], u[11], u[12], u[13],
            u[14], u[15]
        )
    });
    StoreImage {
        image,
        base_address: file.base_address(),
        exports_trie: file.exports_trie(),
        uuid,
        sections: file
            .sections()
            .iter()
            .map(|section| ImageSection {
                name: section.name.clone(),
                segment: section.segment.clone(),
                address: section.address,
                size: section.size,
                flags: section.flags,
            })
            .collect(),
        segments: file
            .segments()
            .map(|segment| ImageSegment {
                name: segment.name.clone(),
                address: segment.address,
                memory_size: segment.memory_size,
                offset: segment.offset,
            })
            .collect(),
        dependencies: file
            .dependencies()
            .filter_map(|(command, name, flags)| {
                let kind = match command {
                    macho::LC_LOAD_DYLIB => DependencyKind::Load,
                    macho::LC_LOAD_WEAK_DYLIB => DependencyKind::Weak,
                    macho::LC_REEXPORT_DYLIB => DependencyKind::ReExport,
                    macho::LC_LOAD_UPWARD_DYLIB => DependencyKind::Upward,
                    macho::LC_LAZY_LOAD_DYLIB => DependencyKind::LazyLoad,
                    _ => return None,
                };
                Some(Dependency {
                    kind,
                    name: name.to_owned(),
                    flags,
                })
            })
            .collect(),
    }
}

fn read_system_file(extractor: &apfs::Extractor<'_>, path: &str) -> Result<Vec<u8>> {
    let file = extractor.open_file(path)?;
    if file.size > MAXIMUM_SYSTEM_FILE_SIZE {
        return Err(Error::msg("store system file exceeds size limit"));
    }
    let mut data = vec![0u8; usize::try_from(file.size).unwrap_or(usize::MAX)];
    file.read_at(&mut data, 0)?;
    Ok(data)
}

struct LocalFile(std::fs::File);

impl ReadAt for LocalFile {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()> {
        #[cfg(unix)]
        std::os::unix::fs::FileExt::read_exact_at(&self.0, data, offset)?;
        #[cfg(windows)]
        {
            let mut done = 0;
            while done < data.len() {
                let count = std::os::windows::fs::FileExt::seek_read(
                    &self.0,
                    &mut data[done..],
                    offset + done as u64,
                )?;
                if count == 0 {
                    return Err(Error::msg("EOF"));
                }
                done += count;
            }
        }
        Ok(())
    }
}
