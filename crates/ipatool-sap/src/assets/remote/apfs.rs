//! Port of Go `assets/apfs.go` with the parts of `majd/go-apfs` it uses: reading files
//! from a sealed, raw APFS container through its object maps and extent trees.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use super::{ReadAt, le16, le32, le64};
use crate::error::{Context, Error, Result};
use crate::machimage;

const BLOCK_SIZE: u64 = 4096;
const BLOCK_LENGTH: usize = 4096;
const OBJECT_HEADER_SIZE: usize = 32;
const OBJECT_TYPE_NX_SUPERBLOCK: u32 = 0x01;
const OBJECT_TYPE_BTREE: u32 = 0x02;
const OBJECT_TYPE_NODE: u32 = 0x03;
const OBJECT_TYPE_OMAP: u32 = 0x0b;
const OBJECT_TYPE_FS: u32 = 0x0d;
const OBJECT_TYPE_FSTREE: u32 = 0x0e;
const OBJECT_TYPE_FEXT_TREE: u32 = 0x1f;
const OBJECT_NO_HEADER: u32 = 0x2000_0000;
const NODE_ROOT: u16 = 0x0001;
const NODE_LEAF: u16 = 0x0002;
const NODE_FIXED_KV_SIZE: u16 = 0x0004;
const NODE_HASHED: u16 = 0x0008;
const NODE_NO_HEADER: u16 = 0x0010;
const MAXIMUM_TREE_DEPTH: usize = 32;
const NODE_HEADER_END: usize = 56;
const BTREE_INFO_SIZE: usize = 40;

const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
const OBJ_TYPE_SHIFT: u32 = 60;
const FSROOT_OID: u64 = 2;
const APFS_TYPE_EXTENT: u8 = 2;
const APFS_TYPE_INODE: u8 = 3;
const APFS_TYPE_XATTR: u8 = 4;
const APFS_TYPE_FILE_EXTENT: u8 = 8;
const APFS_TYPE_DIR_REC: u8 = 9;
const APFS_TYPE_SNAP_METADATA: u8 = 1;
const APFS_TYPE_SNAP_NAME: u8 = 11;
const APFS_TYPE_MAX_VALID: u8 = 13;
const INO_EXT_TYPE_DSTREAM: u8 = 8;
const XATTR_DATA_EMBEDDED: u16 = 0x0002;
const INODE_VALUE_SIZE: usize = 92;
const FILE_EXTENT_LENGTH_MASK: u64 = 0x00ff_ffff_ffff_ffff;
const DECMPFS_XATTR_NAME: &str = "com.apple.decmpfs";
const RESOURCE_FORK_XATTR_NAME: &str = "com.apple.ResourceFork";

const DECMPFS_LZVN: u32 = 8;
const DECMPFS_LZFSE: u32 = 12;
const DECMPFS_LZBITMAP: u32 = 14;
const DECMPFS_BLOCK_SIZE: u64 = 64 << 10;
const MAXIMUM_CACHED_BLOCKS: usize = 128;

/// A raw container read through `ReadAt`, like Go's `readerDevice`.
struct Device<'a> {
    source: &'a dyn ReadAt,
    size: u64,
}

impl Device<'_> {
    fn read_raw_block(&self, address: u64) -> Result<Vec<u8>> {
        let mut block = vec![0u8; BLOCK_LENGTH];
        let offset = address
            .checked_mul(BLOCK_SIZE)
            .ok_or_else(|| Error::msg("Apple filesystem block address overflows"))?;
        self.source
            .read_at(&mut block, offset)
            .context("read Apple filesystem block")?;
        Ok(block)
    }

    /// Reads a block as an object. Sealed tree nodes omit the object header; after their
    /// hash was checked they get a synthetic one so they decode like any other node.
    fn read_object_block(&self, address: u64) -> Result<Vec<u8>> {
        let mut block = self.read_raw_block(address)?;
        if block[..OBJECT_HEADER_SIZE].iter().all(|byte| *byte == 0) {
            let flags = le16(&block[OBJECT_HEADER_SIZE..]);
            if flags & NODE_NO_HEADER != 0 {
                let object_type = if flags & NODE_ROOT != 0 {
                    OBJECT_TYPE_BTREE
                } else {
                    OBJECT_TYPE_NODE
                };
                block[8..16].copy_from_slice(&address.to_le_bytes());
                block[24..28].copy_from_slice(&(OBJECT_NO_HEADER | object_type).to_le_bytes());
                block[28..32].copy_from_slice(&OBJECT_TYPE_FSTREE.to_le_bytes());
                let checksum = create_checksum(&block);
                block[..8].copy_from_slice(&checksum.to_le_bytes());
            }
        }
        Ok(block)
    }

    /// Port of go-apfs `types.ReadObj` for the object kinds this extractor walks.
    fn read_object(&self, address: u64) -> Result<Object> {
        let block = self.read_object_block(address).with_context(|| {
            format!("failed to read object block data at block address {address:#x}")
        })?;
        if !verify_checksum(&block) {
            return Err(Error::msg(
                "obj_phys_t data block failed checksum validation: bad block checksum",
            ));
        }
        Ok(Object { block })
    }

    fn read_node(&self, address: u64) -> Result<Node> {
        let object = self.read_object(address)?;
        match object.object_type() {
            OBJECT_TYPE_BTREE | OBJECT_TYPE_NODE => Node::parse(&object),
            other => Err(Error::msg(format!(
                "object at {address:#x} is type {other:#x}, not a B-tree node"
            ))),
        }
    }
}

struct Object {
    block: Vec<u8>,
}

impl Object {
    fn xid(&self) -> u64 {
        le64(&self.block[16..])
    }

    fn object_type(&self) -> u32 {
        le32(&self.block[24..]) & 0x0000_ffff
    }

    fn subtype(&self) -> u32 {
        le32(&self.block[28..])
    }

    fn field(&self, offset: usize) -> u64 {
        le64(&self.block[offset..])
    }
}

#[derive(Clone, Copy)]
struct OMapEntry {
    oid: u64,
    xid: u64,
    /// The child block for index nodes, the mapped object's block for leaves.
    paddr: u64,
}

#[derive(Clone, Copy)]
struct FextEntry {
    private_id: u64,
    logical_address: u64,
    length: u64,
    physical_block: u64,
}

#[derive(Clone)]
struct Record {
    id_and_type: u64,
    key: Vec<u8>,
    value: Vec<u8>,
}

impl Record {
    fn id(&self) -> u64 {
        self.id_and_type & OBJ_ID_MASK
    }

    fn record_type(&self) -> u8 {
        u8::try_from(self.id_and_type >> OBJ_TYPE_SHIFT).unwrap_or(u8::MAX)
    }
}

#[derive(Clone)]
enum Entries {
    OMap(Vec<OMapEntry>),
    Fext(Vec<FextEntry>),
    Records(Vec<Record>),
}

#[derive(Clone)]
struct Node {
    flags: u16,
    level: u16,
    entries: Entries,
}

impl Node {
    fn is_leaf(&self) -> bool {
        self.flags & NODE_LEAF != 0
    }

    fn hashed(&self) -> bool {
        self.flags & NODE_HASHED != 0
    }

    fn parse(object: &Object) -> Result<Self> {
        let block = &object.block;
        let flags = le16(&block[32..]);
        let level = le16(&block[34..]);
        let count = le32(&block[36..]) as usize;
        let table_length = usize::from(le16(&block[42..]));
        let value_end = block.len()
            - if flags & NODE_ROOT != 0 {
                BTREE_INFO_SIZE
            } else {
                0
            };
        let mut locations = Vec::with_capacity(count);
        for index in 0..count {
            let location = if flags & NODE_FIXED_KV_SIZE != 0 {
                let at = NODE_HEADER_END + index * 4;
                let entry = block
                    .get(at..at + 4)
                    .ok_or_else(|| Error::msg("failed to read offsets: unexpected EOF"))?;
                (
                    usize::from(le16(entry)),
                    None,
                    usize::from(le16(&entry[2..])),
                    None,
                )
            } else {
                let at = NODE_HEADER_END + index * 8;
                let entry = block
                    .get(at..at + 8)
                    .ok_or_else(|| Error::msg("failed to read offsets: unexpected EOF"))?;
                (
                    usize::from(le16(entry)),
                    Some(usize::from(le16(&entry[2..]))),
                    usize::from(le16(&entry[4..])),
                    Some(usize::from(le16(&entry[6..]))),
                )
            };
            locations.push(location);
        }
        let key_at = |offset: usize, length: usize| -> Result<&[u8]> {
            let start = NODE_HEADER_END + table_length + offset;
            block
                .get(start..start + length)
                .ok_or_else(|| Error::msg("B-tree key extends beyond its node"))
        };
        let value_at = |offset: usize, length: usize| -> Result<&[u8]> {
            let start = value_end
                .checked_sub(offset)
                .ok_or_else(|| Error::msg("B-tree value extends beyond its node"))?;
            block
                .get(start..start + length)
                .ok_or_else(|| Error::msg("B-tree value extends beyond its node"))
        };
        let omap_value_size = if level > 0 { 8 } else { 16 };
        let entries = match object.subtype() {
            OBJECT_TYPE_OMAP => Entries::OMap(
                locations
                    .iter()
                    .map(|&(key, _, value, _)| {
                        Ok(omap_entry(
                            key_at(key, 16)?,
                            value_at(value, omap_value_size)?,
                            level,
                        ))
                    })
                    .collect::<Result<_>>()
                    .context("failed to read omap node entry")?,
            ),
            OBJECT_TYPE_FEXT_TREE if level > 0 => Entries::OMap(
                locations
                    .iter()
                    .map(|&(key, _, value, _)| {
                        Ok(omap_entry(key_at(key, 16)?, value_at(value, 8)?, level))
                    })
                    .collect::<Result<_>>()
                    .context("failed to read omap node entry")?,
            ),
            OBJECT_TYPE_FEXT_TREE => Entries::Fext(
                locations
                    .iter()
                    .map(|&(key, _, value, _)| {
                        let key = key_at(key, 16)?;
                        let (length, physical_block) = if value == 0xffff {
                            (0, 0)
                        } else {
                            let value = value_at(value, 16)?;
                            (le64(value) & FILE_EXTENT_LENGTH_MASK, le64(&value[8..]))
                        };
                        Ok(FextEntry {
                            private_id: le64(key),
                            logical_address: le64(&key[8..]),
                            length,
                            physical_block,
                        })
                    })
                    .collect::<Result<_>>()
                    .context("failed to read fext node entry")?,
            ),
            _ => Entries::Records(
                locations
                    .iter()
                    .map(|&(key, key_length, value, value_length)| {
                        let key = key_at(key, key_length.unwrap_or(8))?;
                        let value = if value == 0xffff {
                            &[][..]
                        } else {
                            value_at(value, value_length.unwrap_or(0))?
                        };
                        let record = Record {
                            id_and_type: le64(key),
                            key: key.to_vec(),
                            value: value.to_vec(),
                        };
                        if record.record_type() == 0 || record.record_type() > APFS_TYPE_MAX_VALID {
                            return Err(Error::msg(format!(
                                "got unsupported APFS type {}",
                                record.record_type()
                            )));
                        }
                        Ok(record)
                    })
                    .collect::<Result<_>>()
                    .context("failed to read node entry")?,
            ),
        };
        Ok(Self {
            flags,
            level,
            entries,
        })
    }

    /// Port of go-apfs `BTreeNodePhys.GetOMapEntry`.
    fn omap_entry(&self, device: &Device<'_>, oid: u64, max_xid: u64) -> Result<OMapEntry> {
        let no_records = || Error::msg("no matching records exist in this B-tree");
        let mut node = self.clone();
        loop {
            let Entries::OMap(entries) = &node.entries else {
                return Err(Error::msg("object map node holds no object map entries"));
            };
            let mut found = *entries.last().ok_or_else(no_records)?;
            for (index, entry) in entries.iter().enumerate() {
                if entry.oid > oid || (entry.oid == oid && entry.xid > max_xid) {
                    found = *index
                        .checked_sub(1)
                        .and_then(|previous| entries.get(previous))
                        .ok_or_else(no_records)?;
                    break;
                }
            }
            if node.is_leaf() {
                if found.oid != oid || found.xid > max_xid {
                    return Err(no_records());
                }
                return Ok(found);
            }
            node = device
                .read_node(found.paddr)
                .with_context(|| format!("failed to read B-tree node at {:#x}", found.paddr))
                .context("failed to read omap child")?;
        }
    }
}

fn omap_entry(key: &[u8], value: &[u8], level: u16) -> OMapEntry {
    OMapEntry {
        oid: le64(key),
        xid: le64(&key[8..]),
        paddr: if level > 0 {
            le64(value)
        } else {
            le64(&value[8..])
        },
    }
}

/// go-apfs `CreateChecksum`, over the whole block including the stored checksum.
fn create_checksum(data: &[u8]) -> u64 {
    const MODULUS: u64 = (2 << 31) - 1;
    let (mut sum1, mut sum2) = (0u64, 0u64);
    for word in data.as_chunks::<4>().0 {
        sum1 = (sum1 + u64::from(u32::from_le_bytes(*word))) % MODULUS;
        sum2 = (sum2 + sum1) % MODULUS;
    }
    let check1 = MODULUS - ((sum1 + sum2) % MODULUS);
    let check2 = MODULUS - ((sum1 + check1) % MODULUS);
    (check2 << 32) | check1
}

/// go-apfs `VerifyChecksum`: it only rejects blocks whose running sums are both zero.
fn verify_checksum(data: &[u8]) -> bool {
    const MODULUS: u64 = (2 << 31) - 1;
    let (mut sum1, mut sum2) = (0u64, 0u64);
    for word in data.as_chunks::<4>().0 {
        sum1 = (sum1 + u64::from(u32::from_le_bytes(*word))) % MODULUS;
        sum2 = (sum2 + sum1) % MODULUS;
    }
    ((sum2 << 32) | sum1) != 0
}

#[derive(Clone, Copy)]
struct Extent {
    logical: u64,
    physical: u64,
    length: u64,
}

pub(super) struct Extractor<'a> {
    device: Device<'a>,
    fs_root: Node,
    volume_omap: Node,
    root_tree_base: u64,
    transaction_id: u64,
    fext_tree_oid: u64,
}

impl<'a> Extractor<'a> {
    pub(super) fn open(source: &'a dyn ReadAt, size: u64) -> Result<Self> {
        if size < 36 {
            return Err(Error::msg(format!(
                "apple filesystem image is too small: {size}"
            )));
        }
        let mut header = [0u8; 36];
        source
            .read_at(&mut header, 0)
            .context("read Apple filesystem header")?;
        if &header[32..36] != b"NXSB" {
            return Err(Error::msg(
                "open Apple disk image: the pinned image is not a raw APFS container",
            ));
        }
        let device = Device { source, size };
        Self::mount(device).context("open Apple filesystem")
    }

    /// Port of go-apfs `NewAPFS` for a single-volume container.
    fn mount(device: Device<'a>) -> Result<Self> {
        let container = device
            .read_object(0)
            .context("failed to read APFS container")?;
        if container.object_type() != OBJECT_TYPE_NX_SUPERBLOCK
            || &container.block[32..36] != b"NXSB"
        {
            return Err(Error::msg(
                "failed to read APFS container: not an nx_superblock_t",
            ));
        }
        let descriptor_blocks = le32(&container.block[104..]);
        if descriptor_blocks >> 31 != 0 {
            return Err(Error::msg(
                "failed to find the container superblock that has the largest transaction identifier and isnʼt malformed: unable to parse non-contiguous checkpoint descriptor area",
            ));
        }
        let descriptor_base = container.field(112);
        let mut latest = None;
        for index in 0..u64::from(descriptor_blocks) {
            match device.read_object(descriptor_base + index) {
                Ok(object) => latest = Some(object),
                Err(error) if error.to_string().contains("bad block checksum") => {}
                Err(error) => {
                    return Err(error).context("failed to read XpDescBlock").context(
                        "failed to find the container superblock that has the largest transaction identifier and isnʼt malformed",
                    );
                }
            }
        }
        let valid = latest
            .filter(|object| object.object_type() == OBJECT_TYPE_NX_SUPERBLOCK)
            .ok_or_else(|| Error::msg("apple filesystem has no valid container superblock"))?;
        let container_omap = read_omap_tree(&device, valid.field(160))?;
        let Entries::OMap(volumes) = &container_omap.entries else {
            return Err(Error::msg(
                "apple filesystem container has an invalid object map",
            ));
        };
        let [volume_entry] = volumes.as_slice() else {
            return Err(Error::msg(
                "apple filesystem container does not hold exactly one volume",
            ));
        };
        let volume = device
            .read_object(volume_entry.paddr)
            .context("failed to read APFS omap.tree.entry.omap (volume)")?;
        if volume.object_type() != OBJECT_TYPE_FS || &volume.block[32..36] != b"APSB" {
            return Err(Error::msg(
                "apple filesystem volume has an invalid superblock",
            ));
        }
        let omap_object = device
            .read_object(volume.field(0x80))
            .context("failed to read root node of the container object map B-tree")?;
        let volume_omap = omap_tree(&device, &omap_object)?;
        let root_tree_oid = volume.field(0x88);
        let fs_root_entry = volume_omap
            .omap_entry(&device, root_tree_oid, volume.xid())
            .context("failed to get root entry")?;
        let fs_root = device
            .read_node(fs_root_entry.paddr)
            .context("failed to read root btree")?;
        Ok(Self {
            fs_root,
            volume_omap,
            root_tree_base: root_tree_oid,
            transaction_id: omap_object.xid(),
            fext_tree_oid: volume.field(0x408),
            device,
        })
    }

    pub(super) fn open_file(&self, path: &str) -> Result<File<'_>> {
        let records = self.resolve_path(path)?;
        let mut inode = None;
        let mut compression = None;
        let mut resource = None;
        let mut extents = Vec::new();
        for record in &records {
            match record.record_type() {
                APFS_TYPE_INODE => inode = Some(&record.value),
                APFS_TYPE_FILE_EXTENT => extents.push(file_extent(record)),
                APFS_TYPE_XATTR => {
                    let (name, flags, data) = xattr(record)?;
                    match name.as_str() {
                        DECMPFS_XATTR_NAME => {
                            if flags & XATTR_DATA_EMBEDDED == 0 || data.len() < 16 {
                                return Err(Error::msg(
                                    "read compression metadata: unexpected EOF",
                                ));
                            }
                            compression = Some((le32(&data[4..]), le64(&data[8..])));
                        }
                        RESOURCE_FORK_XATTR_NAME
                            if flags & XATTR_DATA_EMBEDDED == 0 && data.len() >= 16 =>
                        {
                            resource = Some((le64(data), le64(&data[8..])));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        let inode = inode.ok_or_else(|| Error::msg("file has no inode record"))?;
        if inode.len() < INODE_VALUE_SIZE {
            return Err(Error::msg("file has no inode record"));
        }

        if let Some((method, uncompressed_size)) = compression {
            if method != DECMPFS_LZVN && method != DECMPFS_LZFSE && method != DECMPFS_LZBITMAP {
                return Err(Error::msg(format!("unsupported compression type {method}")));
            }
            let (object_id, size) =
                resource.ok_or_else(|| Error::msg("compressed file has no resource fork"))?;
            let extents = self
                .data_extents(object_id)
                .context("read resource-fork extents")?;
            let fork = ExtentReader::new(&self.device, extents, size)?;
            let reader = DecmpfsReader::new(fork, size, uncompressed_size, method)?;
            return Ok(File {
                stream: Stream::Compressed(reader),
                size: uncompressed_size,
            });
        }

        let size = inode_stream_size(inode).unwrap_or_else(|| le64(&inode[84..]));
        if extents.is_empty() && size != 0 {
            extents = self
                .data_extents(le64(&inode[8..]))
                .context("read file extents")?;
        }
        Ok(File {
            stream: Stream::Plain(ExtentReader::new(&self.device, extents, size)?),
            size,
        })
    }

    fn resolve_path(&self, path: &str) -> Result<Vec<Record>> {
        let mut object_id = FSROOT_OID;
        for name in path.trim_matches('/').split('/') {
            let records = self.records_for_object(object_id)?;
            object_id = records
                .iter()
                .filter(|record| record.record_type() == APFS_TYPE_DIR_REC)
                .find(|record| drec_name(&record.key) == Some(name) && record.value.len() >= 8)
                .map(|record| le64(&record.value))
                .ok_or_else(|| Error::msg(format!("path component {name:?} was not found")))?;
        }
        self.records_for_object(object_id)
    }

    fn records_for_object(&self, object_id: u64) -> Result<Vec<Record>> {
        self.collect_records(&self.fs_root, object_id, &mut HashSet::new(), 0)
    }

    fn collect_records(
        &self,
        node: &Node,
        object_id: u64,
        visited: &mut HashSet<u64>,
        depth: usize,
    ) -> Result<Vec<Record>> {
        if depth > MAXIMUM_TREE_DEPTH {
            return Err(Error::msg("filesystem tree exceeds maximum depth"));
        }
        let Entries::Records(entries) = &node.entries else {
            return Err(Error::msg(
                "filesystem tree contains an invalid index entry",
            ));
        };
        if node.is_leaf() {
            return Ok(entries
                .iter()
                .filter(|record| record.id() == object_id)
                .cloned()
                .collect());
        }
        let mut records = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let lower = entry.id();
            let upper = entries.get(index + 1).map_or(u64::MAX, Record::id);
            if object_id < lower || object_id > upper {
                continue;
            }
            let (child, address) = self.filesystem_child(node, entry)?;
            if !visited.insert(address) {
                continue;
            }
            records.extend(self.collect_records(&child, object_id, visited, depth + 1)?);
        }
        Ok(records)
    }

    fn filesystem_child(&self, node: &Node, entry: &Record) -> Result<(Node, u64)> {
        let record_type = entry.record_type();
        if matches!(
            record_type,
            APFS_TYPE_SNAP_METADATA | APFS_TYPE_SNAP_NAME | APFS_TYPE_EXTENT
        ) || node.level == 0
        {
            return Err(Error::msg(
                "sealed filesystem tree contains an invalid child reference",
            ));
        }
        let (object_id, expected_hash) = if node.hashed() {
            if entry.value.len() < 40 {
                return Err(Error::msg(
                    "sealed filesystem tree contains an invalid child reference",
                ));
            }
            (
                self.root_tree_base.wrapping_add(le64(&entry.value)),
                Some(&entry.value[8..40]),
            )
        } else {
            if entry.value.len() < 8 {
                return Err(Error::msg(
                    "sealed filesystem tree contains an invalid child reference",
                ));
            }
            (le64(&entry.value), None)
        };
        let mapping = self
            .volume_omap
            .omap_entry(&self.device, object_id, self.transaction_id)
            .with_context(|| format!("resolve filesystem child {object_id:#x}"))?;
        if let Some(expected) = expected_hash {
            let raw = self
                .device
                .read_raw_block(mapping.paddr)
                .with_context(|| format!("read filesystem child {object_id:#x}"))?;
            if Sha256::digest(&raw)[..] != *expected {
                return Err(Error::msg(format!(
                    "filesystem child {object_id:#x} failed SHA-256 verification"
                )));
            }
        }
        let child = self
            .device
            .read_node(mapping.paddr)
            .with_context(|| format!("decode filesystem child {object_id:#x}"))?;
        Ok((child, mapping.paddr))
    }

    fn data_extents(&self, object_id: u64) -> Result<Vec<Extent>> {
        if self.fext_tree_oid != 0 {
            return self.physical_extents(object_id);
        }
        let mut extents: Vec<Extent> = self
            .records_for_object(object_id)?
            .iter()
            .filter(|record| record.record_type() == APFS_TYPE_FILE_EXTENT)
            .map(file_extent)
            .collect();
        extents.sort_by_key(|extent| extent.logical);
        Ok(extents)
    }

    fn physical_extents(&self, object_id: u64) -> Result<Vec<Extent>> {
        let root = self
            .device
            .read_node(self.fext_tree_oid)
            .context("read physical extent tree")?;
        let mut extents =
            self.collect_physical_extents(&root, object_id, &mut HashSet::new(), 0)?;
        extents.sort_by_key(|extent| extent.logical);
        Ok(extents)
    }

    fn collect_physical_extents(
        &self,
        node: &Node,
        object_id: u64,
        visited: &mut HashSet<u64>,
        depth: usize,
    ) -> Result<Vec<Extent>> {
        if depth > MAXIMUM_TREE_DEPTH {
            return Err(Error::msg("physical extent tree exceeds maximum depth"));
        }
        if node.is_leaf() {
            let Entries::Fext(entries) = &node.entries else {
                return Ok(Vec::new());
            };
            return Ok(entries
                .iter()
                .filter(|entry| entry.private_id == object_id)
                .map(|entry| Extent {
                    logical: entry.logical_address,
                    physical: entry.physical_block,
                    length: entry.length,
                })
                .collect());
        }
        let Entries::OMap(entries) = &node.entries else {
            return Err(Error::msg(
                "physical extent tree contains an invalid index entry",
            ));
        };
        let mut extents = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let upper = entries.get(index + 1).map_or(u64::MAX, |next| next.oid);
            if object_id < entry.oid || object_id > upper || !visited.insert(entry.paddr) {
                continue;
            }
            let child = self
                .device
                .read_node(entry.paddr)
                .with_context(|| format!("read physical extent child {:#x}", entry.paddr))?;
            extents.extend(self.collect_physical_extents(&child, object_id, visited, depth + 1)?);
        }
        Ok(extents)
    }

    /// The file at `path`, reduced to its arm64e slice unless `thin` is false.
    fn image_file(&self, path: &str, thin: bool) -> Result<Vec<u8>> {
        let file = self.open_file(path)?;
        let (offset, size) = if thin {
            machimage::arm64_range(&mut |data, offset| file.read_at(data, offset), file.size)
                .context("select runtime image architecture")?
        } else {
            (0, file.size)
        };
        let mut data = vec![
            0u8;
            usize::try_from(size)
                .map_err(|_| Error::msg("copy file data: file is too large"))?
        ];
        file.read_at(&mut data, offset).context("copy file data")?;
        Ok(data)
    }
}

/// Extracts each path, keyed by its file name.
pub(super) fn extract_apfs_files(
    source: &dyn ReadAt,
    size: u64,
    paths: &[&str],
) -> Result<HashMap<String, Vec<u8>>> {
    let extractor = Extractor::open(source, size)?;
    let mut files = HashMap::with_capacity(paths.len());
    for path in paths {
        let name = path.rsplit('/').next().unwrap_or(path);
        let data = extractor
            .image_file(
                path,
                std::path::Path::new(name).extension() != Some("icxs".as_ref()),
            )
            .with_context(|| format!("extract {path} from Apple filesystem"))?;
        files.insert(name.to_owned(), data);
    }
    Ok(files)
}

fn read_omap_tree(device: &Device<'_>, address: u64) -> Result<Node> {
    let object = device.read_object(address)?;
    omap_tree(device, &object)
}

/// The tree of an `omap_phys_t`; like go-apfs, a snapshot tree replaces the main tree.
fn omap_tree(device: &Device<'_>, object: &Object) -> Result<Node> {
    if object.object_type() != OBJECT_TYPE_OMAP {
        return Err(Error::msg(
            "apple filesystem volume has an invalid object map",
        ));
    }
    let tree = match (object.field(0x30), object.field(0x38)) {
        (_, snapshot) if snapshot > 0 => snapshot,
        (tree, _) if tree > 0 => tree,
        _ => return Err(Error::msg("apple filesystem volume has no object-map tree")),
    };
    device
        .read_node(tree)
        .with_context(|| format!("failed to read omap_phys_t tree at block {tree:#x}"))
}

fn file_extent(record: &Record) -> Extent {
    let (key, value) = (&record.key, &record.value);
    Extent {
        logical: key.get(8..16).map_or(0, le64),
        physical: value.get(8..16).map_or(0, le64),
        length: value.get(..8).map_or(0, le64) & FILE_EXTENT_LENGTH_MASK,
    }
}

fn drec_name(key: &[u8]) -> Option<&str> {
    let length = usize::try_from(key.get(8..12).map(le32)? & 0x3ff).ok()?;
    let name = key.get(12..12 + length)?;
    std::str::from_utf8(name)
        .ok()
        .map(|name| name.trim_matches('\0'))
}

fn xattr(record: &Record) -> Result<(String, u16, &[u8])> {
    let (key, value) = (&record.key, &record.value);
    let invalid = || Error::msg("invalid extended attribute record");
    let name_length = usize::from(key.get(8..10).map(le16).ok_or_else(invalid)?);
    let name = key.get(10..10 + name_length).ok_or_else(invalid)?;
    let flags = value.get(..2).map(le16).ok_or_else(invalid)?;
    let length = usize::from(value.get(2..4).map(le16).ok_or_else(invalid)?);
    let data = if flags & XATTR_DATA_EMBEDDED != 0 {
        value.get(4..4 + length).ok_or_else(invalid)?
    } else {
        value.get(4..).ok_or_else(invalid)?
    };
    Ok((
        String::from_utf8_lossy(name).trim_matches('\0').to_owned(),
        flags,
        data,
    ))
}

/// The size of the first `INO_EXT_TYPE_DSTREAM` extended field.
fn inode_stream_size(value: &[u8]) -> Option<u64> {
    if value.len() <= INODE_VALUE_SIZE {
        return None;
    }
    let count = usize::from(le16(value.get(INODE_VALUE_SIZE..)?));
    let mut data = INODE_VALUE_SIZE + 4 + count * 4;
    for index in 0..count {
        let header = value.get(INODE_VALUE_SIZE + 4 + index * 4..)?;
        let size = usize::from(le16(header.get(2..)?));
        if header[0] == INO_EXT_TYPE_DSTREAM {
            return value.get(data..data + 8).map(le64);
        }
        data += size.next_multiple_of(8);
    }
    None
}

pub(super) struct File<'a> {
    stream: Stream<'a>,
    pub(super) size: u64,
}

enum Stream<'a> {
    Plain(ExtentReader<'a>),
    Compressed(DecmpfsReader<'a>),
}

impl ReadAt for File<'_> {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()> {
        if offset > self.size || data.len() as u64 > self.size - offset {
            return Err(Error::msg("EOF"));
        }
        match &self.stream {
            Stream::Plain(reader) => reader.read_at(data, offset),
            Stream::Compressed(reader) => reader.read_at(data, offset),
        }
    }
}

struct ExtentReader<'a> {
    device: &'a Device<'a>,
    extents: Vec<Extent>,
    size: u64,
}

impl<'a> ExtentReader<'a> {
    fn new(device: &'a Device<'a>, mut extents: Vec<Extent>, size: u64) -> Result<Self> {
        if size > i64::MAX as u64 {
            return Err(Error::msg(format!(
                "file size {size} exceeds the supported maximum"
            )));
        }
        extents.sort_by_key(|extent| extent.logical);
        let mut position = 0u64;
        for extent in &extents {
            if extent.length == 0
                || extent.logical != position
                || extent.physical > u64::MAX / BLOCK_SIZE
            {
                return Err(Error::msg("file has invalid or sparse physical extents"));
            }
            let physical_offset = extent.physical * BLOCK_SIZE;
            if physical_offset > device.size || extent.length > device.size - physical_offset {
                return Err(Error::msg("file extent exceeds the Apple filesystem image"));
            }
            position = position
                .checked_add(extent.length)
                .ok_or_else(|| Error::msg("file extent length overflows"))?;
        }
        if position < size {
            return Err(Error::msg(format!(
                "file extents cover {position} bytes, expected {size}"
            )));
        }
        Ok(Self {
            device,
            extents,
            size,
        })
    }
}

impl ReadAt for ExtentReader<'_> {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()> {
        if offset > self.size || data.len() as u64 > self.size - offset {
            return Err(Error::msg("EOF"));
        }
        let mut position = offset;
        let mut written = 0;
        while written < data.len() {
            let index = self
                .extents
                .partition_point(|extent| extent.logical + extent.length <= position);
            let extent = self
                .extents
                .get(index)
                .filter(|extent| position >= extent.logical)
                .ok_or_else(|| Error::msg("unexpected EOF"))?;
            let available = extent.logical + extent.length - position;
            let count = usize::try_from(available.min((data.len() - written) as u64))
                .unwrap_or(data.len() - written);
            let physical = extent.physical * BLOCK_SIZE + position - extent.logical;
            self.device
                .source
                .read_at(&mut data[written..written + count], physical)?;
            written += count;
            position += count as u64;
        }
        Ok(())
    }
}

struct DecmpfsReader<'a> {
    source: ExtentReader<'a>,
    output_size: u64,
    method: u32,
    offsets: Vec<u32>,
    cache: Mutex<BlockCache>,
}

#[derive(Default)]
struct BlockCache {
    blocks: HashMap<u64, Arc<Vec<u8>>>,
    order: VecDeque<u64>,
}

impl<'a> DecmpfsReader<'a> {
    fn new(
        source: ExtentReader<'a>,
        source_size: u64,
        output_size: u64,
        method: u32,
    ) -> Result<Self> {
        if output_size > i64::MAX as u64 {
            return Err(Error::msg(format!(
                "compressed file size {output_size} exceeds the supported maximum"
            )));
        }
        if source_size < 8 {
            return Err(Error::msg("compressed resource fork is too small"));
        }
        let mut first = [0u8; 4];
        source
            .read_at(&mut first, 0)
            .context("read compression index")?;
        let table_size = u64::from(u32::from_le_bytes(first));
        let block_count = output_size.div_ceil(DECMPFS_BLOCK_SIZE);
        let expected_table_size = (block_count + 1) * 4;
        if table_size != expected_table_size || table_size > source_size {
            return Err(Error::msg(format!(
                "compression index has size {table_size}, expected {expected_table_size}"
            )));
        }
        let mut table = vec![0u8; usize::try_from(table_size).unwrap_or(0)];
        source
            .read_at(&mut table, 0)
            .context("read compression index")?;
        let offsets = super::le32_words(&table);
        for (index, window) in offsets.windows(2).enumerate() {
            let (start, end) = (u64::from(window[0]), u64::from(window[1]));
            if start < table_size
                || end <= start
                || end > source_size
                || end - start > DECMPFS_BLOCK_SIZE + 1
            {
                return Err(Error::msg(format!(
                    "compressed block {index} has invalid range {start}..{end}"
                )));
            }
        }
        let last = offsets.last().copied().unwrap_or_default();
        if u64::from(last) != source_size {
            return Err(Error::msg(format!(
                "compressed blocks end at {last}, expected {source_size}"
            )));
        }
        Ok(Self {
            source,
            output_size,
            method,
            offsets,
            cache: Mutex::new(BlockCache::default()),
        })
    }

    fn block(&self, index: u64) -> Result<Arc<Vec<u8>>> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(block) = cache.blocks.get(&index) {
            return Ok(block.clone());
        }
        let slot = usize::try_from(index).unwrap_or(usize::MAX);
        let (Some(&start), Some(&end)) = (self.offsets.get(slot), self.offsets.get(slot + 1))
        else {
            return Err(Error::msg("EOF"));
        };
        let mut compressed = vec![0u8; (end - start) as usize];
        self.source
            .read_at(&mut compressed, u64::from(start))
            .with_context(|| format!("read compressed block {index}"))?;
        let expected = DECMPFS_BLOCK_SIZE.min(self.output_size - index * DECMPFS_BLOCK_SIZE);
        let expected = usize::try_from(expected).unwrap_or(usize::MAX);
        let plain = decompress_block(&compressed, self.method, expected)
            .with_context(|| format!("decode compressed block {index}"))?;
        if plain.len() != expected {
            return Err(Error::msg(format!(
                "decompressed block {index} has size {}, expected {expected}",
                plain.len()
            )));
        }
        if cache.order.len() == MAXIMUM_CACHED_BLOCKS
            && let Some(oldest) = cache.order.pop_front()
        {
            cache.blocks.remove(&oldest);
        }
        let plain = Arc::new(plain);
        cache.blocks.insert(index, plain.clone());
        cache.order.push_back(index);
        Ok(plain)
    }
}

impl ReadAt for DecmpfsReader<'_> {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()> {
        if offset > self.output_size || data.len() as u64 > self.output_size - offset {
            return Err(Error::msg("EOF"));
        }
        let mut position = offset;
        let mut written = 0;
        while written < data.len() {
            let block = self.block(position / DECMPFS_BLOCK_SIZE)?;
            let inside = usize::try_from(position % DECMPFS_BLOCK_SIZE).unwrap_or(0);
            let count = (data.len() - written).min(block.len() - inside);
            data[written..written + count].copy_from_slice(&block[inside..inside + count]);
            written += count;
            position += count as u64;
        }
        Ok(())
    }
}

fn decompress_block(compressed: &[u8], method: u32, maximum_size: usize) -> Result<Vec<u8>> {
    let Some(&first) = compressed.first() else {
        return Err(Error::msg("compressed block is empty"));
    };
    match method {
        DECMPFS_LZVN if first == 0x06 => Ok(compressed[1..].to_vec()),
        DECMPFS_LZVN => {
            super::lzvn::decompress(compressed, maximum_size).context("decompress LZVN block")
        }
        DECMPFS_LZFSE if first != b'b' => Ok(compressed[1..].to_vec()),
        DECMPFS_LZFSE => {
            super::lzfse_decode(compressed, maximum_size).context("decompress LZFSE block")
        }
        DECMPFS_LZBITMAP if first == 0xff => Ok(compressed[1..].to_vec()),
        DECMPFS_LZBITMAP => {
            super::lzbitmap::decompress(compressed).context("decompress LZBITMAP block")
        }
        _ => Err(Error::msg(format!("unsupported compression type {method}"))),
    }
}
