use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::error::{Context, Error, Result};

use super::fixups::{self, ChainedFixups, Kind};
use super::macho::{self, Bind, DyldInfo, Rebase, Section, Segment, Symbol};
use super::preparation_cache::{
    ChainedRelocation, IMAGE_PREPARATIONS, Instructions, Preparation, PreparationCache,
};
use super::{Memory, PAGE_SIZE, POINTER_SIZE, add, align, arm64_slice};

const MAX_IMAGE_SPAN: u64 = 1 << 30;
const REBASE_TYPE_POINTER: u8 = 1;
const BIND_TYPE_POINTER: u8 = 1;

/// Resolves an imported symbol name to a guest address.
pub type Resolve<'r> = &'r mut dyn FnMut(&str) -> Result<u64>;

/// A Mach-O image prepared for loading into guest memory.
pub struct Image<'a> {
    pub(super) name: String,
    pub(super) data: Cow<'a, [u8]>,
    pub(super) modified: Option<HashMap<u64, Vec<u8>>>,
    pub(super) instructions: Arc<Instructions>,
    pub(super) base: u64,
    pub(super) segments: Vec<Segment>,
    pub(super) sections: Vec<Section>,
    pub(super) dyld_info: Option<DyldInfo>,
    pub(super) symbols: Option<Arc<Vec<Symbol>>>,
    pub(super) binds: Arc<Vec<Bind>>,
    pub(super) rebases: Arc<Vec<Rebase>>,
    pub(super) chained: Option<ChainedFixups>,
    pub(super) fixups: Arc<Vec<ChainedRelocation>>,
    pub(super) relocated: bool,
    pub(super) loaded_base: u64,
}

impl Image<'static> {
    /// Opens a private copy of the arm64e image in `input`.
    pub fn open(name: &str, input: &[u8]) -> Result<Self> {
        let data = arm64_slice(input).with_context(|| format!("open {name}"))?;
        Image::open_image(name, Cow::Owned(data.to_vec()), None, &IMAGE_PREPARATIONS)
    }
}

impl<'a> Image<'a> {
    /// Borrows immutable image bytes and copies only pages changed by relocation.
    /// Instruction patches are applied while loading into guest memory.
    pub fn open_shared(name: &str, input: &'a [u8]) -> Result<Self> {
        Image::open_shared_in(name, input, &IMAGE_PREPARATIONS)
    }

    /// [`Image::open_shared`] with its own preparation cache, so tests do not depend on
    /// what other tests left in the process-wide one.
    pub(super) fn open_shared_in(
        name: &str,
        input: &'a [u8],
        preparations: &PreparationCache,
    ) -> Result<Self> {
        let data = arm64_slice(input).with_context(|| format!("open {name}"))?;
        Image::open_image(
            name,
            Cow::Borrowed(data),
            Some(HashMap::new()),
            preparations,
        )
    }

    pub(super) fn empty(
        name: &str,
        data: Cow<'a, [u8]>,
        modified: Option<HashMap<u64, Vec<u8>>>,
    ) -> Self {
        Self {
            name: name.to_owned(),
            data,
            modified,
            instructions: Arc::default(),
            base: 0,
            segments: Vec::new(),
            sections: Vec::new(),
            dyld_info: None,
            symbols: None,
            binds: Arc::default(),
            rebases: Arc::default(),
            chained: None,
            fixups: Arc::default(),
            relocated: false,
            loaded_base: 0,
        }
    }

    fn open_image(
        name: &str,
        data: Cow<'a, [u8]>,
        modified: Option<HashMap<u64, Vec<u8>>>,
        preparations: &PreparationCache,
    ) -> Result<Self> {
        let shared = modified.is_some();
        let mut image = Image::empty(name, data, modified);

        let mut key = [0u8; 32];
        let mut preparation = None;
        if shared {
            key = Sha256::digest(&image.data).into();
            preparation = preparations.get(&key);
        }

        let file = macho::File::parse(&image.data, preparation.is_none())
            .with_context(|| format!("open {name} Mach-O"))?;
        if !macho::is_arm64(file.cpu) {
            return Err(Error::msg(format!(
                "open {name}: expected arm64 Mach-O, found {}",
                macho::cpu_name(file.cpu)
            )));
        }

        image.base = file.base_address();
        image.segments.clone_from(&file.segments);
        image.sections.clone_from(&file.sections);
        image.dyld_info = file.dyld_info;
        image.validate_segments()?;

        if let Some(preparation) = preparation {
            image.symbols = preparation.symbols;
            image.instructions = preparation.instructions;
            image.fixups = preparation.fixups;
            image.binds = preparation.binds;
            image.rebases = preparation.rebases;
            return Ok(image);
        }
        image.symbols = file.symtab.clone().map(Arc::new);

        image.remove_pointer_authentication()?;
        image.read_fixups(&file)?;

        if shared {
            if image.chained.is_some() {
                image.fixups = Arc::new(image.decode_chained_relocations()?);
                image.chained = None;
            }
            preparations.put(
                key,
                Preparation {
                    instructions: Arc::clone(&image.instructions),
                    fixups: Arc::clone(&image.fixups),
                    binds: Arc::clone(&image.binds),
                    rebases: Arc::clone(&image.rebases),
                    symbols: image.symbols.clone(),
                },
            );
        }

        Ok(image)
    }

    fn read_fixups(&mut self, file: &macho::File) -> Result<()> {
        let name = self.name.as_str();
        if let Some((offset, size)) = file.chained_fixups {
            let blob = usize::try_from(offset)
                .ok()
                .zip(usize::try_from(size).ok())
                .and_then(|(start, size)| self.data.get(start..start.checked_add(size)?))
                .ok_or_else(|| {
                    Error::msg(format!(
                        "read {name} chained fixups: failed to read DyldChainedFixups data at offset={offset:#x}; unexpected EOF"
                    ))
                })?;
            let mut chained = ChainedFixups::read(blob, &file.segments)
                .with_context(|| format!("read {name} chained fixups"))?;
            chained
                .walk(&|offset, out| self.read_at(offset, out))
                .with_context(|| format!("parse {name} chained fixups"))?;
            self.chained = Some(chained);
            return Ok(());
        }

        let read_pointer = |offset: u64| self.read_pointer(offset);
        let binds = macho::binds(file, &read_pointer, &self.data);
        let rebases = macho::rebases(file, &read_pointer, &self.data);
        if let Some(binds) = binds {
            self.binds = Arc::new(binds.with_context(|| format!("read {name} bindings"))?);
        }
        if let Some(rebases) = rebases {
            self.rebases = Arc::new(rebases.with_context(|| format!("read {name} rebases"))?);
        }
        Ok(())
    }

    #[must_use]
    pub fn base(&self) -> u64 {
        self.base
    }

    fn remove_pointer_authentication(&mut self) -> Result<()> {
        let mut ranges = Vec::new();
        for section in &self.sections {
            if !section.has_instructions() {
                continue;
            }
            let start = u64::from(section.offset);
            match add(start, section.size) {
                Some(end) if end <= self.data.len() as u64 => ranges.push((start, end)),
                _ => {
                    return Err(Error::msg(format!(
                        "instruction section {} exceeds {}",
                        section.name, self.name
                    )));
                }
            }
        }

        for (start, end) in ranges {
            let mut offset = start;
            while offset + 4 <= end {
                let at = usize::try_from(offset).unwrap_or(usize::MAX);
                let word = &self.data[at..at + 4];
                let instruction = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
                let replacement = without_pointer_authentication(instruction);
                if replacement != instruction {
                    self.write_instruction(offset, replacement);
                }
                offset += 4;
            }
        }
        Ok(())
    }

    pub fn export(&self, name: &str, load_base: u64) -> Result<u64> {
        let address = macho::find_symbol_address(
            self.symbols.as_deref().map(Vec::as_slice),
            self.dyld_info,
            self.base,
            &self.data,
            name,
        )
        .with_context(|| format!("find {name} in {}", self.name))?;
        if address < self.base {
            return Err(Error::msg(format!(
                "symbol {name} in {} precedes image base",
                self.name
            )));
        }
        add(load_base, address - self.base)
            .ok_or_else(|| Error::msg(format!("symbol {name} address overflows in {}", self.name)))
    }

    pub fn relocate(&mut self, load_base: u64, resolve: Resolve<'_>) -> Result<()> {
        if self.relocated {
            return Err(Error::msg(format!("{} is already relocated", self.name)));
        }

        let rebases = Arc::clone(&self.rebases);
        for relocation in rebases.iter() {
            if relocation.typ != REBASE_TYPE_POINTER {
                return Err(Error::msg(format!(
                    "{} uses unsupported rebase type {}",
                    self.name, relocation.typ
                )));
            }
            if relocation.value < self.base {
                return Err(Error::msg(format!(
                    "{} contains a rebase below its image base",
                    self.name
                )));
            }
            let offset =
                self.segment_file_offset(&relocation.segment, relocation.offset, POINTER_SIZE)?;
            let address = add(load_base, relocation.value - self.base)
                .ok_or_else(|| Error::msg(format!("rebase address overflows in {}", self.name)))?;
            self.put_pointer(offset, address)?;
        }

        let binds = Arc::clone(&self.binds);
        for binding in binds.iter() {
            // Lazy bind streams may omit SET_TYPE; dyld's default is a pointer bind.
            if binding.typ != 0 && binding.typ != BIND_TYPE_POINTER {
                return Err(Error::msg(format!(
                    "{} uses unsupported bind type {} for {}",
                    self.name, binding.typ, binding.name
                )));
            }
            let offset =
                self.segment_file_offset(&binding.segment, binding.seg_offset, POINTER_SIZE)?;
            let address = resolve(&binding.name)
                .with_context(|| format!("resolve {} for {}", binding.name, self.name))?;
            let address = add_signed(address, binding.addend)
                .with_context(|| format!("apply addend for {} in {}", binding.name, self.name))?;
            self.put_pointer(offset, address)?;
        }

        self.relocate_chained(load_base, resolve)?;

        self.relocated = true;
        self.loaded_base = load_base;
        Ok(())
    }

    /// Decodes authenticated pointers embedded in an executable extracted from a kernel
    /// collection. These are already fixed up for the collection and are not described by
    /// the image's standalone Mach-O bind/rebase streams.
    pub fn relocate_kernel_cache_pointers(
        &mut self,
        load_base: u64,
        kernel_base: u64,
        resolve: Resolve<'_>,
    ) -> Result<()> {
        const KERNEL_FIXUP_STRIDE: u64 = 4;

        let mut external_symbols = HashMap::new();
        if let Some(symbols) = &self.symbols {
            for symbol in symbols.iter() {
                if symbol.value == 0 || self.contains_address(symbol.value) {
                    continue;
                }
                external_symbols.insert(symbol.value, symbol.name.clone());
            }
        }

        let segments = self.segments.clone();
        for item in &segments {
            if matches!(item.name.as_str(), "__TEXT" | "__TEXT_EXEC" | "__LINKEDIT")
                || item.file_size < POINTER_SIZE
            {
                continue;
            }
            let mut offset = item.file_off;
            let remainder = item.address % KERNEL_FIXUP_STRIDE;
            if remainder != 0 {
                offset += KERNEL_FIXUP_STRIDE - remainder;
            }
            let end = item.file_off + item.file_size;
            while offset <= end - POINTER_SIZE {
                let raw = self.pointer_at(offset);
                let target = fixups::bits(raw, 0, 30);
                if fixups::bits(raw, 30, 2) != 0 || target == 0 {
                    offset += KERNEL_FIXUP_STRIDE;
                    continue;
                }
                let Some(original) = add(kernel_base, target) else {
                    offset += KERNEL_FIXUP_STRIDE;
                    continue;
                };
                let relocated = if self.contains_address(original) {
                    add(load_base, original - self.base).ok_or_else(|| {
                        Error::msg(format!(
                            "kernel-cache pointer at {offset:#x} overflows in {}",
                            self.name
                        ))
                    })?
                } else if let Some(name) = external_symbols.get(&original) {
                    resolve(name).with_context(|| {
                        format!(
                            "resolve kernel-cache pointer {name} at {offset:#x} in {}",
                            self.name
                        )
                    })?
                } else {
                    offset += KERNEL_FIXUP_STRIDE;
                    continue;
                };
                self.put_pointer(offset, relocated)?;
                offset += KERNEL_FIXUP_STRIDE;
            }
        }
        Ok(())
    }

    fn contains_address(&self, address: u64) -> bool {
        self.segments
            .iter()
            .any(|item| address >= item.address && address - item.address < item.size)
    }

    pub(super) fn decode_chained_relocations(&mut self) -> Result<Vec<ChainedRelocation>> {
        let Some(mut chained) = self.chained.take() else {
            return Ok(Vec::new());
        };
        let result = self.decode_chained(&mut chained);
        self.chained = Some(chained);
        result
    }

    fn decode_chained(&self, chained: &mut ChainedFixups) -> Result<Vec<ChainedRelocation>> {
        let length = self.data.len() as u64;
        let fixups = chained.fixups.clone();
        let mut relocations = Vec::with_capacity(fixups.len());
        for fixup in &fixups {
            let offset = fixup.offset;
            if offset > length || POINTER_SIZE > length - offset {
                return Err(Error::msg(format!(
                    "chained fixup at {offset:#x} exceeds {}",
                    self.name
                )));
            }
            relocations.push(self.decode_fixup(chained, fixup)?);
        }
        Ok(relocations)
    }

    fn decode_fixup(
        &self,
        chained: &mut ChainedFixups,
        fixup: &fixups::Fixup,
    ) -> Result<ChainedRelocation> {
        let offset = fixup.offset;
        let raw = fixup.raw;
        let mut relocation = ChainedRelocation {
            offset,
            ..ChainedRelocation::default()
        };
        match fixup.kind {
            Kind::Ptr64RebaseOffset => {
                // DYLD_CHAINED_PTR_64_OFFSET encodes an image-relative target. go-macho's
                // Rebase subtracts the preferred address from that offset a second time.
                relocation.target = fixups::bits(raw, 36, 8) << 56 | fixups::bits(raw, 0, 36);
            }
            Kind::Arm64eRebase => {
                relocation.target = fixups::bits(raw, 0, 43);
                if chained.pointer_format == fixups::DYLD_CHAINED_PTR_ARM64E {
                    let absolute = (fixups::bits(raw, 43, 8) << 56 | fixups::bits(raw, 0, 43))
                        & 0x00ff_ffff_ffff_ffff;
                    if absolute < self.base {
                        return Err(Error::msg(format!(
                            "chained rebase at {offset:#x} precedes the base of {}",
                            self.name
                        )));
                    }
                    relocation.target = absolute - self.base;
                }
            }
            Kind::Arm64eAuthRebase => relocation.target = fixups::bits(raw, 0, 32),
            Kind::Ptr64Rebase
            | Kind::KernelCacheRebase
            | Kind::Ptr32Rebase
            | Kind::Ptr32CacheRebase
            | Kind::Ptr32FirmwareRebase => {
                relocation.target = chained.rebase(fixup, self.base).with_context(|| {
                    format!("decode chained rebase at {offset:#x} in {}", self.name)
                })?;
            }
            Kind::Arm64eBind
            | Kind::Arm64eAuthBind
            | Kind::Arm64eBind24
            | Kind::Arm64eAuthBind24
            | Kind::Ptr64Bind
            | Kind::Ptr32Bind => {
                let (symbol, ordinal) = chained.bind_import(fixup);
                relocation.is_bind = true;
                relocation.symbol = symbol;
                let mut addend = match fixup.kind {
                    Kind::Arm64eBind | Kind::Arm64eBind24 => {
                        sign_extend_19(fixups::bits(raw, 32, 19))
                    }
                    Kind::Ptr64Bind => fixups::bits(raw, 24, 8).cast_signed(),
                    Kind::Ptr32Bind => fixups::bits(raw, 20, 6).cast_signed(),
                    _ => 0,
                };
                if let Some(import) = usize::try_from(ordinal)
                    .ok()
                    .and_then(|index| chained.imports.get(index))
                {
                    addend = addend.wrapping_add(import.addend);
                }
                relocation.addend = addend;
            }
        }
        Ok(relocation)
    }

    pub(super) fn relocate_chained(&mut self, load_base: u64, resolve: Resolve<'_>) -> Result<()> {
        let relocations = if self.chained.is_some() {
            Arc::new(self.decode_chained_relocations()?)
        } else {
            Arc::clone(&self.fixups)
        };

        for relocation in relocations.iter() {
            let address = if relocation.is_bind {
                let value = resolve(&relocation.symbol)
                    .with_context(|| format!("resolve {} for {}", relocation.symbol, self.name))?;
                add_signed(value, relocation.addend).with_context(|| {
                    format!(
                        "apply chained addend for {} in {}",
                        relocation.symbol, self.name
                    )
                })?
            } else {
                add(load_base, relocation.target).ok_or_else(|| {
                    Error::msg(format!(
                        "chained rebase at {:#x} with target {:#x} overflows in {}",
                        relocation.offset, relocation.target, self.name
                    ))
                })?
            };
            self.put_pointer(relocation.offset, address)?;
        }
        Ok(())
    }

    /// Prepares an image extracted from a dyld shared cache. Such images have already had
    /// their cache slide applied, so they stay at their preferred addresses. `external`
    /// maps cache symbol addresses to the import names implemented by the emulator.
    pub fn prepare_prelinked(
        &mut self,
        external: &[(u64, &str)],
        resolve: Resolve<'_>,
    ) -> Result<()> {
        if self.relocated {
            return Err(Error::msg(format!("{} is already relocated", self.name)));
        }

        let mut patched = HashMap::with_capacity(external.len());
        for &(address, name) in external {
            let value =
                resolve(name).with_context(|| format!("resolve {name} for {}", self.name))?;
            patched.insert(address, value);
        }

        if !patched.is_empty() {
            let segments = self.segments.clone();
            for item in &segments {
                if item.name == "__TEXT"
                    || item.name == "__LINKEDIT"
                    || item.file_size < POINTER_SIZE
                {
                    continue;
                }
                let mut offset = item.file_off;
                let remainder = item.address % POINTER_SIZE;
                if remainder != 0 {
                    offset += POINTER_SIZE - remainder;
                }
                let end = item.file_off + item.file_size;
                while offset <= end - POINTER_SIZE {
                    if let Some(&value) = patched.get(&self.pointer_at(offset)) {
                        self.put_pointer(offset, value)?;
                    }
                    offset += POINTER_SIZE;
                }
            }
        }

        self.relocated = true;
        self.loaded_base = self.base;
        Ok(())
    }

    pub fn load(&self, memory: &mut (impl Memory + ?Sized)) -> Result<()> {
        if !self.relocated {
            return Err(Error::msg(format!(
                "{} must be relocated before loading",
                self.name
            )));
        }

        self.map_segments(memory)?;

        let mut buffer = Vec::new();
        if self.has_overlay() {
            buffer = vec![0; (128 << 10).min(self.data.len())];
        }

        for item in &self.segments {
            if item.name == "__PAGEZERO" || item.file_size == 0 {
                continue;
            }
            let end = match add(item.file_off, item.file_size) {
                Some(end) if end <= self.data.len() as u64 => end,
                _ => {
                    return Err(Error::msg(format!(
                        "segment {} data exceeds {}",
                        item.name, self.name
                    )));
                }
            };
            let address =
                add(self.loaded_base, item.address.wrapping_sub(self.base)).ok_or_else(|| {
                    Error::msg(format!(
                        "segment {} address overflows in {}",
                        item.name, self.name
                    ))
                })?;
            self.write_segment(memory, address, item.file_off, end, &mut buffer)
                .with_context(|| format!("write segment {} from {}", item.name, self.name))?;
        }
        Ok(())
    }

    fn map_segments(&self, memory: &mut (impl Memory + ?Sized)) -> Result<()> {
        let mut ranges = Vec::with_capacity(self.segments.len());
        for item in &self.segments {
            if item.name == "__PAGEZERO" || item.size == 0 {
                continue;
            }
            if item.address < self.base {
                return Err(Error::msg(format!(
                    "segment {} in {} precedes image base",
                    item.name, self.name
                )));
            }
            let address = add(self.loaded_base, item.address - self.base).ok_or_else(|| {
                Error::msg(format!(
                    "segment {} address overflows in {}",
                    item.name, self.name
                ))
            })?;
            let end = add(address, item.size).ok_or_else(|| {
                Error::msg(format!(
                    "segment {} range overflows in {}",
                    item.name, self.name
                ))
            })?;
            ranges.push((address - address % PAGE_SIZE, align(end, PAGE_SIZE)));
        }

        if ranges.is_empty() {
            return Err(Error::msg(format!(
                "{} has no loadable segments",
                self.name
            )));
        }

        ranges.sort_by_key(|range| range.0);
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
        for current in ranges {
            if let Some(last) = merged.last_mut()
                && current.0 <= last.1
            {
                last.1 = last.1.max(current.1);
                continue;
            }
            merged.push(current);
        }

        let mut mapped = 0u64;
        for (start, end) in merged {
            let size = end.wrapping_sub(start);
            mapped = mapped.wrapping_add(size);
            if mapped > MAX_IMAGE_SPAN {
                return Err(Error::msg(format!(
                    "mapped segments make {} too large",
                    self.name
                )));
            }
            memory
                .mem_map(start, size)
                .with_context(|| format!("map {}", self.name))?;
        }
        Ok(())
    }

    pub(super) fn validate_segments(&self) -> Result<()> {
        for item in &self.segments {
            if item.file_size > item.size {
                return Err(Error::msg(format!(
                    "segment {} file data exceeds its memory size in {}",
                    item.name, self.name
                )));
            }
            if add(item.address, item.size).is_none() {
                return Err(Error::msg(format!(
                    "segment {} address range overflows in {}",
                    item.name, self.name
                )));
            }
            match add(item.file_off, item.file_size) {
                Some(end) if end <= self.data.len() as u64 => {}
                _ => {
                    return Err(Error::msg(format!(
                        "segment {} data exceeds {}",
                        item.name, self.name
                    )));
                }
            }
        }
        Ok(())
    }

    pub(super) fn segment_file_offset(&self, name: &str, offset: u64, size: u64) -> Result<u64> {
        let Some(item) = self.segments.iter().find(|item| item.name == name) else {
            return Err(Error::msg(format!(
                "fixup references unknown segment {name} in {}",
                self.name
            )));
        };
        let segment_end = match add(offset, size) {
            Some(end) if end <= item.size => end,
            _ => {
                return Err(Error::msg(format!(
                    "fixup at {offset:#x} exceeds segment {name} in {}",
                    self.name
                )));
            }
        };
        if segment_end > item.file_size {
            return Err(Error::msg(format!(
                "fixup at {offset:#x} exceeds file data for segment {name} in {}",
                self.name
            )));
        }
        let result = add(item.file_off, offset)
            .ok_or_else(|| Error::msg(format!("fixup offset overflows in {}", self.name)))?;
        match add(result, size) {
            Some(end) if end <= self.data.len() as u64 => Ok(result),
            _ => Err(Error::msg(format!(
                "fixup at {result:#x} exceeds {}",
                self.name
            ))),
        }
    }

    pub(super) fn put_pointer(&mut self, offset: u64, value: u64) -> Result<()> {
        match add(offset, 8) {
            Some(end) if end <= self.data.len() as u64 => {
                self.write_at(offset, &value.to_le_bytes());
                Ok(())
            }
            _ => Err(Error::msg(format!(
                "fixup at {offset:#x} exceeds {}",
                self.name
            ))),
        }
    }

    fn read_pointer(&self, offset: u64) -> Result<u64> {
        let mut value = [0u8; 8];
        match self.read_at(offset, &mut value) {
            8 => Ok(u64::from_le_bytes(value)),
            0 => Err(Error::msg("EOF")),
            _ => Err(Error::msg("unexpected EOF")),
        }
    }

    /// The immutable symbol table used for guest exports and ivars.
    pub fn symbols(&self) -> &[Symbol] {
        self.symbols.as_deref().map_or(&[], Vec::as_slice)
    }

    /// The guest's executable section address ranges.
    #[must_use]
    pub fn instruction_ranges(&self) -> Vec<[u64; 2]> {
        self.sections
            .iter()
            .filter(|section| section.has_instructions())
            .map(|section| [section.address, section.address.wrapping_add(section.size)])
            .collect()
    }
}

const fn sign_extend_19(addend: u64) -> i64 {
    if addend & 0x40000 != 0 {
        (addend | 0xffff_ffff_fffc_0000).cast_signed()
    } else {
        addend.cast_signed()
    }
}

pub(super) fn add_signed(value: u64, delta: i64) -> Result<u64> {
    if delta >= 0 {
        return add(value, delta.unsigned_abs()).ok_or_else(|| Error::msg("address overflow"));
    }
    let magnitude = delta.unsigned_abs();
    if magnitude > value {
        return Err(Error::msg("address underflow"));
    }
    Ok(value - magnitude)
}

/// Rewrites `ARM64e` authentication operations for a VM with canonical, unsigned pointers.
/// Other instructions are unchanged.
#[must_use]
pub const fn without_pointer_authentication(instruction: u32) -> u32 {
    const NOP: u32 = 0xd503_201f;

    match instruction & 0xffff_fc00 {
        0xd71f_0800 | 0xd71f_0c00 | 0xd61f_0800 | 0xd61f_0c00 => {
            0xd61f_0000 | (instruction & 0x3e0)
        }
        0xd73f_0800 | 0xd73f_0c00 | 0xd63f_0800 | 0xd63f_0c00 => {
            0xd63f_0000 | (instruction & 0x3e0)
        }
        0xdac1_0000 | 0xdac1_0400 | 0xdac1_0800 | 0xdac1_0c00 | 0xdac1_1000 | 0xdac1_1400
        | 0xdac1_1800 | 0xdac1_1c00 => NOP,
        _ => match instruction {
            0xd65f_0bff | 0xd65f_0fff => 0xd65f_03c0,
            0xd503_211f | 0xd503_215f | 0xd503_219f | 0xd503_21df | 0xd503_233f | 0xd503_237f
            | 0xd503_23bf | 0xd503_23ff | 0xd503_20ff => NOP,
            _ => match instruction & 0xffff_ffe0 {
                0xdac1_23e0 | 0xdac1_27e0 | 0xdac1_2be0 | 0xdac1_2fe0 | 0xdac1_33e0
                | 0xdac1_37e0 | 0xdac1_3be0 | 0xdac1_3fe0 | 0xdac1_43e0 | 0xdac1_47e0 => NOP,
                _ => instruction,
            },
        },
    }
}
