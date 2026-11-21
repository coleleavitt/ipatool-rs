//! Port of Go `assets/dyld.go`: one image exported from the arm64e dyld shared cache.

use std::collections::HashMap;

use super::apfs::File;
use super::macho;
use super::{ReadAt, le32, le64};
use crate::error::{Context, Error, Result};

const DYLD_HEADER_SIZE: usize = 568;
const DYLD_MAPPING_SIZE: u64 = 56;
const DYLD_MAPPING_LENGTH: usize = 56;
const DYLD_IMAGE_INFO_SIZE: u64 = 32;
const DYLD_IMAGE_INFO_LENGTH: usize = 32;
const DYLD_SUBCACHE_ENTRY_SIZE: u64 = 56;
const DYLD_SUBCACHE_ENTRY_LENGTH: usize = 56;
const DYLD_MAXIMUM_MAPPINGS: u64 = 64;
const DYLD_MAXIMUM_IMAGES: u64 = 100_000;
const DYLD_MAXIMUM_SUBCACHES: u64 = 128;
const DYLD_MAXIMUM_PATH_LENGTH: u64 = 4096;
const DYLD_SLIDE_INFO5_SIZE: u64 = 24;
const DYLD_SLIDE_INFO5_LENGTH: usize = 24;
const DYLD_MACH_HEADER64_SIZE: u64 = 32;
const DYLD_MACH_HEADER64_LENGTH: usize = 32;
const DYLD_MACH_SEGMENT64_COMMAND: u32 = 0x19;
pub(super) const DYLD_MAXIMUM_LOAD_COMMANDS_SIZE: u64 = 16 << 20;
const DYLD_MAXIMUM_SLIDE_CHAIN_POINTERS: usize = 1 << 16;

pub(super) struct Cache<'a> {
    pub(super) members: Vec<Member<'a>>,
    slide5: SlideInfo5,
}

pub(super) struct Member<'a> {
    pub(super) file: Box<dyn ReadAt + 'a>,
    pub(super) size: u64,
    uuid: [u8; 16],
    mappings: Vec<Mapping>,
}

#[derive(Clone, Copy)]
pub(super) struct Mapping {
    pub(super) member: usize,
    address: u64,
    size: u64,
    file_offset: u64,
    slide_info_offset: u64,
    slide_info_size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SlideInfo5 {
    page_size: u32,
    page_starts_count: u32,
    value_add: u64,
}

pub(super) struct Image {
    pub(super) member: usize,
    pub(super) offset: u64,
}

struct Rebase {
    address: u64,
    target: u64,
}

/// One opened shared-cache member file.
pub(super) struct MemberFile<'a> {
    pub(super) reader: Box<dyn ReadAt + 'a>,
    pub(super) size: u64,
}

impl<'a> From<File<'a>> for MemberFile<'a> {
    fn from(file: File<'a>) -> Self {
        Self {
            size: file.size,
            reader: Box::new(file),
        }
    }
}

/// Opens the member with the given suffix (`""` is the primary cache file).
pub(super) type Opener<'o, 'a> = dyn Fn(&str) -> Result<MemberFile<'a>> + 'o;

pub(super) fn extract_dyld_image_from_files(
    opener: &Opener<'_, '_>,
    image_path: &str,
) -> Result<Vec<u8>> {
    Cache::open(opener)
        .context("open shared cache")?
        .extract_image(image_path)
}

impl<'a> Cache<'a> {
    /// Go's `extractDyldImageFromFiles` after the cache is open.
    pub(super) fn extract_image(&self, image_path: &str) -> Result<Vec<u8>> {
        let image = self
            .image(image_path)
            .with_context(|| format!("find {image_path} in shared cache"))?;
        let mut file = self
            .open_image(&image)
            .with_context(|| format!("read {image_path} from shared cache"))?;
        self.image_symbols(&image, &mut file)
            .with_context(|| format!("read {image_path} symbols"))?;
        let reader = CacheReader {
            cache: self,
            linkedit_member: self.linkedit_member(&image)?,
        };
        let mut exported = file
            .export(&reader, &[])
            .with_context(|| format!("export {image_path} from shared cache"))?;
        self.apply_slide(&mut exported)
            .with_context(|| format!("apply {image_path} shared-cache pointers"))?;
        Ok(exported)
    }

    pub(super) fn open(opener: &Opener<'_, 'a>) -> Result<Self> {
        let (primary, header) = open_member(opener, "", 0)?;
        let mut members = vec![primary];
        let subcache_offset = u64::from(le32(&header[392..]));
        let subcache_count = u64::from(le32(&header[396..]));
        if subcache_count > DYLD_MAXIMUM_SUBCACHES
            || subcache_count > 0
                && !valid_range(
                    members[0].size,
                    subcache_offset,
                    subcache_count * DYLD_SUBCACHE_ENTRY_SIZE,
                )
        {
            return Err(Error::msg("shared cache has an invalid subcache table"));
        }
        let mut entries = vec![0u8; to_usize(subcache_count * DYLD_SUBCACHE_ENTRY_SIZE)?];
        if !entries.is_empty() {
            members[0]
                .file
                .read_at(&mut entries, subcache_offset)
                .context("read shared-cache subcache table")?;
        }
        for entry in entries.as_chunks::<DYLD_SUBCACHE_ENTRY_LENGTH>().0 {
            let suffix = String::from_utf8_lossy(&entry[24..56])
                .trim_end_matches('\0')
                .to_owned();
            if suffix.is_empty() || suffix.contains(['/', '\\']) {
                return Err(Error::msg("shared cache has an invalid subcache suffix"));
            }
            let (member, _) = open_member(opener, &suffix, members.len())
                .with_context(|| format!("open shared-cache member {suffix}"))?;
            if member.uuid[..] != entry[..16] {
                return Err(Error::msg(format!(
                    "shared-cache member {suffix} has an unexpected UUID"
                )));
            }
            members.push(member);
        }

        let mut cache = Self {
            members,
            slide5: SlideInfo5 {
                page_size: 0,
                page_starts_count: 0,
                value_add: 0,
            },
        };
        let mut slide5 = None;
        for member in &cache.members {
            for mapping in &member.mappings {
                if mapping.slide_info_size == 0 {
                    continue;
                }
                let version = cache.read_slide_version(mapping)?;
                if version != 5 {
                    return Err(Error::msg(format!(
                        "unsupported shared-cache slide version {version}"
                    )));
                }
                let slide = cache.read_slide_info5(mapping)?;
                match slide5 {
                    None => slide5 = Some(slide),
                    Some(existing)
                        if existing.page_size != slide.page_size
                            || existing.value_add != slide.value_add =>
                    {
                        return Err(Error::msg(
                            "shared-cache members use incompatible slide information",
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
        cache.slide5 = slide5.ok_or_else(|| Error::msg("shared cache has no slide information"))?;
        Ok(cache)
    }

    fn image(&self, path: &str) -> Result<Image> {
        let primary = &self.members[0];
        let mut header = vec![0u8; DYLD_HEADER_SIZE];
        primary
            .file
            .read_at(&mut header, 0)
            .context("read shared-cache header")?;
        let images_offset = u64::from(le32(&header[448..]));
        let images_count = u64::from(le32(&header[452..]));
        if images_count == 0
            || images_count > DYLD_MAXIMUM_IMAGES
            || !valid_range(
                primary.size,
                images_offset,
                images_count * DYLD_IMAGE_INFO_SIZE,
            )
        {
            return Err(Error::msg("shared cache has an invalid image table"));
        }
        let mut images = vec![0u8; to_usize(images_count * DYLD_IMAGE_INFO_SIZE)?];
        primary
            .file
            .read_at(&mut images, images_offset)
            .context("read shared-cache image table")?;
        for data in images.as_chunks::<DYLD_IMAGE_INFO_LENGTH>().0 {
            let address = le64(data);
            let path_offset = u64::from(le32(&data[24..]));
            if read_cstring(primary.file.as_ref(), primary.size, path_offset)? != path {
                continue;
            }
            let mapping = self.mapping_for_address(address)?;
            return Ok(Image {
                member: mapping.member,
                offset: mapping.file_offset + address - mapping.address,
            });
        }
        Err(Error::msg("image was not found"))
    }

    /// Go `StoreCache.AddressRange`: the unslid range covered by every mapping.
    pub(super) fn address_range(&self) -> (u64, u64) {
        let mappings = self.members.iter().flat_map(|member| &member.mappings);
        let start = mappings
            .clone()
            .map(|m| m.address)
            .min()
            .unwrap_or(u64::MAX);
        let end = mappings.map(|m| m.address + m.size).max().unwrap_or(0);
        (start, end.wrapping_sub(start))
    }

    /// Go `StoreCache.Contains`: the range lies inside one mapping.
    pub(super) fn contains(&self, address: u64, size: u64) -> bool {
        self.members
            .iter()
            .flat_map(|member| &member.mappings)
            .any(|m| {
                address >= m.address
                    && address - m.address <= m.size
                    && size <= m.size - (address - m.address)
            })
    }

    /// Go `StoreCache.Images`: every image install name and its header address.
    pub(super) fn image_addresses(&self) -> Result<HashMap<String, u64>> {
        let primary = &self.members[0];
        let mut header = vec![0u8; DYLD_HEADER_SIZE];
        primary
            .file
            .read_at(&mut header, 0)
            .context("read Store cache header")?;
        let offset = u64::from(le32(&header[448..]));
        let count = u64::from(le32(&header[452..]));
        if count == 0
            || count > DYLD_MAXIMUM_IMAGES
            || !valid_range(primary.size, offset, count * DYLD_IMAGE_INFO_SIZE)
        {
            return Err(Error::msg("invalid Store image table"));
        }
        let mut data = vec![0u8; to_usize(count * DYLD_IMAGE_INFO_SIZE)?];
        primary
            .file
            .read_at(&mut data, offset)
            .context("read Store image table")?;
        let mut images = HashMap::with_capacity(data.len() / DYLD_IMAGE_INFO_LENGTH);
        for entry in data.as_chunks::<DYLD_IMAGE_INFO_LENGTH>().0 {
            let name = read_cstring(
                primary.file.as_ref(),
                primary.size,
                u64::from(le32(&entry[24..])),
            )?;
            images.insert(name, le64(entry));
        }
        Ok(images)
    }

    /// The image whose Mach-O header is at `address`.
    pub(super) fn image_at(&self, address: u64) -> Result<Image> {
        let mapping = self.mapping_for_address(address)?;
        Ok(Image {
            member: mapping.member,
            offset: mapping.file_offset + address - mapping.address,
        })
    }

    /// Reads `__LINKEDIT` data of `image` at a load-command file offset.
    pub(super) fn read_linkedit(&self, image: &Image, data: &mut [u8], offset: u64) -> Result<()> {
        self.members[self.linkedit_member(image)?]
            .file
            .read_at(data, offset)
    }

    /// Go `StoreCache.Page`: one unslid 4 KiB page with its shared-cache pointers applied.
    /// `slides` memoizes each slide page's rebases, like Go's `StoreCache.slides`.
    pub(super) fn page(
        &self,
        address: u64,
        slides: &mut HashMap<u64, HashMap<u64, u64>>,
    ) -> Result<Vec<u8>> {
        if address & 4095 != 0 {
            return Err(Error::msg("unaligned Store page"));
        }
        let mapping = self.mapping_for_address(address)?;
        let mut data = vec![0u8; 4096];
        let reader = CacheReader {
            cache: self,
            linkedit_member: 0,
        };
        macho::Source::read_at_address(&reader, &mut data, address).context("read Store page")?;
        if mapping.slide_info_size == 0 {
            return Ok(data);
        }
        let page_size = u64::from(self.read_slide_info5(&mapping)?.page_size);
        let index = (address - mapping.address) / page_size;
        let base = mapping.address + index * page_size;
        let rebases = match slides.entry(base) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(
                self.rebases(&mapping, index, index + 1)?
                    .into_iter()
                    .map(|rebase| (rebase.address, rebase.target))
                    .collect(),
            ),
        };
        for (&location, &target) in rebases.iter() {
            if location >= address && location + 8 <= address + 4096 {
                let start = to_usize(location - address)?;
                data[start..start + 8].copy_from_slice(&target.to_le_bytes());
            }
        }
        Ok(data)
    }

    pub(super) fn open_image(&self, image: &Image) -> Result<macho::File> {
        let member = &self.members[image.member];
        let header = self.image_header(image)?;
        let commands_size = u64::from(le32(&header[20..]));
        if commands_size > DYLD_MAXIMUM_LOAD_COMMANDS_SIZE
            || !valid_range(
                member.size,
                image.offset + DYLD_MACH_HEADER64_SIZE,
                commands_size,
            )
        {
            return Err(Error::msg("image has invalid load commands"));
        }
        let mut data = vec![0u8; to_usize(DYLD_MACH_HEADER64_SIZE + commands_size)?];
        member
            .file
            .read_at(&mut data, image.offset)
            .context("read Mach-O load commands")?;
        macho::File::parse_header_and_commands(&data).context("parse shared-cache image")
    }

    fn image_header(&self, image: &Image) -> Result<[u8; DYLD_MACH_HEADER64_LENGTH]> {
        let mut header = [0u8; DYLD_MACH_HEADER64_LENGTH];
        self.members[image.member]
            .file
            .read_at(&mut header, image.offset)
            .context("read Mach-O header")?;
        if le32(&header) != macho::MAGIC64 {
            return Err(Error::msg("image has an invalid Mach-O magic"));
        }
        Ok(header)
    }

    pub(super) fn linkedit_member(&self, image: &Image) -> Result<usize> {
        let address = self.linkedit_address(image)?;
        Ok(self.mapping_for_address(address)?.member)
    }

    fn linkedit_address(&self, image: &Image) -> Result<u64> {
        let member = &self.members[image.member];
        let header = self.image_header(image)?;
        let command_count = le32(&header[16..]);
        let commands_size = u64::from(le32(&header[20..]));
        if commands_size > DYLD_MAXIMUM_LOAD_COMMANDS_SIZE
            || !valid_range(
                member.size,
                image.offset + DYLD_MACH_HEADER64_SIZE,
                commands_size,
            )
        {
            return Err(Error::msg("image has invalid load commands"));
        }
        let mut commands = vec![0u8; to_usize(commands_size)?];
        member
            .file
            .read_at(&mut commands, image.offset + DYLD_MACH_HEADER64_SIZE)
            .context("read Mach-O load commands")?;
        let mut offset = 0usize;
        for _ in 0..command_count {
            if offset + 8 > commands.len() {
                return Err(Error::msg("image load commands are truncated"));
            }
            let length = le32(&commands[offset + 4..]) as usize;
            if length < 8 || offset + length > commands.len() {
                return Err(Error::msg("image has an invalid load command"));
            }
            if le32(&commands[offset..]) == DYLD_MACH_SEGMENT64_COMMAND
                && length >= 72
                && macho::cstring(&commands[offset + 8..offset + 24]) == "__LINKEDIT"
            {
                return Ok(le64(&commands[offset + 24..]));
            }
            offset += length;
        }
        Err(Error::msg("image has no __LINKEDIT segment"))
    }

    pub(super) fn mapping_for_address(&self, address: u64) -> Result<Mapping> {
        self.members
            .iter()
            .flat_map(|member| &member.mappings)
            .find(|mapping| mapping.address <= address && address < mapping.address + mapping.size)
            .copied()
            .ok_or_else(|| Error::msg(format!("address {address:#x} is outside the shared cache")))
    }

    fn read_slide_version(&self, mapping: &Mapping) -> Result<u32> {
        if mapping.slide_info_size < 4 {
            return Err(Error::msg("shared cache has truncated slide information"));
        }
        let mut data = [0u8; 4];
        self.members[mapping.member]
            .file
            .read_at(&mut data, mapping.slide_info_offset)
            .context("read shared-cache slide version")?;
        Ok(u32::from_le_bytes(data))
    }

    fn read_slide_info5(&self, mapping: &Mapping) -> Result<SlideInfo5> {
        if mapping.slide_info_size < DYLD_SLIDE_INFO5_SIZE {
            return Err(Error::msg("shared cache has truncated slide information"));
        }
        let mut data = [0u8; DYLD_SLIDE_INFO5_LENGTH];
        self.members[mapping.member]
            .file
            .read_at(&mut data, mapping.slide_info_offset)
            .context("read shared-cache slide information")?;
        if le32(&data) != 5 {
            return Err(Error::msg(format!(
                "unsupported shared-cache slide version {}",
                le32(&data)
            )));
        }
        let slide = SlideInfo5 {
            page_size: le32(&data[4..]),
            page_starts_count: le32(&data[8..]),
            value_add: le64(&data[16..]),
        };
        if slide.page_size == 0
            || DYLD_SLIDE_INFO5_SIZE + u64::from(slide.page_starts_count) * 2
                > mapping.slide_info_size
        {
            return Err(Error::msg("shared cache has invalid slide information"));
        }
        Ok(slide)
    }

    /// Writes every slid pointer of the exported image's segments, like Go's `applySlide`
    /// does through `exported.GetOffset` and file writes (which may extend the file).
    fn apply_slide(&self, exported: &mut Vec<u8>) -> Result<()> {
        let parsed =
            macho::File::parse_header_and_commands(exported).context("parse exported image")?;
        let segments: Vec<(u64, u64, u64)> = parsed
            .segments()
            .map(|segment| (segment.address, segment.memory_size, segment.offset))
            .collect();
        for &(address, memory_size, _) in &segments {
            let mapping = self.mapping_for_address(address)?;
            if mapping.slide_info_offset == 0 {
                continue;
            }
            let page_size = u64::from(self.read_slide_info5(&mapping)?.page_size);
            let start = (address - mapping.address) / page_size;
            let end = (address + memory_size - mapping.address + page_size) / page_size;
            for rebase in self.rebases(&mapping, start, end)? {
                let Some(offset) = segments
                    .iter()
                    .find(|(segment, size, _)| {
                        *segment <= rebase.address && rebase.address < segment + size
                    })
                    .map(|(segment, _, offset)| rebase.address - segment + offset)
                else {
                    continue;
                };
                let offset = to_usize(offset)?;
                if exported.len() < offset + 8 {
                    exported.resize(offset + 8, 0);
                }
                exported[offset..offset + 8].copy_from_slice(&rebase.target.to_le_bytes());
            }
        }
        Ok(())
    }

    fn rebases(
        &self,
        mapping: &Mapping,
        start_page: u64,
        mut end_page: u64,
    ) -> Result<Vec<Rebase>> {
        let version = self.read_slide_version(mapping)?;
        if version != 5 {
            return Err(Error::msg(format!(
                "unsupported shared-cache slide version {version}"
            )));
        }
        let slide = self.read_slide_info5(mapping)?;
        let starts = self.read_slide_array(mapping, slide.page_starts_count)?;
        if end_page == 0 || end_page > starts.len() as u64 {
            end_page = starts.len() as u64;
        }
        if start_page > end_page {
            return Err(Error::msg("invalid shared-cache slide page range"));
        }
        let file = &self.members[mapping.member].file;
        let mut rebases = Vec::new();
        for page in start_page..end_page {
            let start = starts[to_usize(page)?];
            if start == 0xffff {
                continue;
            }
            let page_file_offset = mapping.file_offset + page * u64::from(slide.page_size);
            let page_address = mapping.address + page * u64::from(slide.page_size);
            let mut offset = u64::from(start);
            for count in 0..DYLD_MAXIMUM_SLIDE_CHAIN_POINTERS {
                if offset + 8 > u64::from(slide.page_size) {
                    return Err(Error::msg("shared-cache slide chain leaves its page"));
                }
                let mut data = [0u8; 8];
                file.read_at(&mut data, page_file_offset + offset)
                    .context("read shared-cache slide pointer")?;
                let pointer = u64::from_le_bytes(data);
                rebases.push(Rebase {
                    address: page_address + offset,
                    target: slide.decode(pointer),
                });
                let next = pointer >> 52 & 0x7ff;
                if next == 0 {
                    break;
                }
                offset += next * 8;
                if count == DYLD_MAXIMUM_SLIDE_CHAIN_POINTERS - 1 {
                    return Err(Error::msg("shared-cache slide chain is too long"));
                }
            }
        }
        Ok(rebases)
    }

    fn read_slide_array(&self, mapping: &Mapping, count: u32) -> Result<Vec<u16>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut data = vec![0u8; count as usize * 2];
        self.members[mapping.member]
            .file
            .read_at(&mut data, mapping.slide_info_offset + DYLD_SLIDE_INFO5_SIZE)
            .context("read shared-cache slide table")?;
        Ok(data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|value| u16::from_le_bytes(*value))
            .collect())
    }
}

impl SlideInfo5 {
    fn decode(self, pointer: u64) -> u64 {
        if pointer == 0 {
            return 0;
        }
        let mut target = pointer & ((1u64 << 34) - 1);
        if pointer >> 63 == 0 {
            target |= (pointer >> 34 & 0xff) << 56;
        }
        self.value_add + target
    }
}

fn open_member<'a>(
    opener: &Opener<'_, 'a>,
    suffix: &str,
    index: usize,
) -> Result<(Member<'a>, Vec<u8>)> {
    let MemberFile { reader: file, size } = opener(suffix)?;
    if size < DYLD_HEADER_SIZE as u64 {
        return Err(Error::msg("shared-cache member is too small"));
    }
    let mut header = vec![0u8; DYLD_HEADER_SIZE];
    file.read_at(&mut header, 0)?;
    let magic = String::from_utf8_lossy(&header[..16])
        .trim_end_matches('\0')
        .to_owned();
    if magic != "dyld_v1  arm64e" {
        return Err(Error::msg(format!(
            "unsupported shared-cache format {magic:?}"
        )));
    }
    let mapping_offset = u64::from(le32(&header[312..]));
    let mapping_count = u64::from(le32(&header[316..]));
    if mapping_count == 0
        || mapping_count > DYLD_MAXIMUM_MAPPINGS
        || !valid_range(size, mapping_offset, mapping_count * DYLD_MAPPING_SIZE)
    {
        return Err(Error::msg(
            "shared-cache member has an invalid mapping table",
        ));
    }
    let mut mapping_data = vec![0u8; to_usize(mapping_count * DYLD_MAPPING_SIZE)?];
    file.read_at(&mut mapping_data, mapping_offset)?;
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&header[88..104]);
    let mut mappings = Vec::new();
    for data in mapping_data.as_chunks::<DYLD_MAPPING_LENGTH>().0 {
        let mapping = Mapping {
            member: index,
            address: le64(data),
            size: le64(&data[8..]),
            file_offset: le64(&data[16..]),
            slide_info_offset: le64(&data[24..]),
            slide_info_size: le64(&data[32..]),
        };
        if mapping.size == 0
            || mapping.address.checked_add(mapping.size).is_none()
            || !valid_range(size, mapping.file_offset, mapping.size)
            || mapping.slide_info_size != 0
                && !valid_range(size, mapping.slide_info_offset, mapping.slide_info_size)
        {
            return Err(Error::msg("shared-cache member has an invalid mapping"));
        }
        mappings.push(mapping);
    }
    Ok((
        Member {
            file,
            size,
            uuid,
            mappings,
        },
        header,
    ))
}

pub(super) fn valid_range(size: u64, offset: u64, length: u64) -> bool {
    offset <= size && length <= size - offset
}

pub(super) fn read_cstring(file: &dyn ReadAt, size: u64, offset: u64) -> Result<String> {
    if !valid_range(size, offset, 1) {
        return Err(Error::msg("shared cache contains an invalid string offset"));
    }
    let limit = DYLD_MAXIMUM_PATH_LENGTH.min(size - offset);
    let mut data = vec![0u8; to_usize(limit)?];
    file.read_at(&mut data, offset)
        .context("read shared-cache string")?;
    let end = data
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| Error::msg("shared-cache string is not terminated"))?;
    Ok(String::from_utf8_lossy(&data[..end]).into_owned())
}

/// Reads segment data by address across members and image data from the `__LINKEDIT`
/// member, like Go's `dyldCacheReader`.
pub(super) struct CacheReader<'c, 'a> {
    cache: &'c Cache<'a>,
    linkedit_member: usize,
}

impl macho::Source for CacheReader<'_, '_> {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()> {
        self.cache.members[self.linkedit_member]
            .file
            .read_at(data, offset)
    }

    fn read_at_address(&self, data: &mut [u8], mut address: u64) -> Result<()> {
        let mut written = 0;
        while written < data.len() {
            let mapping = self.cache.mapping_for_address(address)?;
            let available = mapping.address + mapping.size - address;
            let count = usize::try_from(available.min((data.len() - written) as u64))
                .unwrap_or(data.len() - written);
            self.cache.members[mapping.member].file.read_at(
                &mut data[written..written + count],
                mapping.file_offset + address - mapping.address,
            )?;
            written += count;
            address += count as u64;
        }
        Ok(())
    }
}

fn to_usize(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| Error::msg("shared-cache offset overflows"))
}
