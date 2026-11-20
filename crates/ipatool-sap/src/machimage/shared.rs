use super::image::Image;
use super::preparation_cache::InstructionPatch;
use super::{Memory, PAGE_SIZE};
use crate::error::{Context, Result};

fn index(offset: u64) -> usize {
    usize::try_from(offset).unwrap_or(usize::MAX)
}

impl Image<'_> {
    pub(super) fn has_overlay(&self) -> bool {
        self.modified
            .as_ref()
            .is_some_and(|pages| !pages.is_empty())
            || !self.instructions.is_empty()
    }

    pub(super) fn write_at(&mut self, mut offset: u64, mut data: &[u8]) {
        if self.modified.is_none() {
            let start = index(offset);
            let bytes = self.data.to_mut();
            let count = data.len().min(bytes.len().saturating_sub(start));
            bytes[start..start + count].copy_from_slice(&data[..count]);
            return;
        }

        while !data.is_empty() {
            let page = offset & !(PAGE_SIZE - 1);
            if !self
                .modified
                .as_ref()
                .is_some_and(|pages| pages.contains_key(&page))
            {
                let end = (page + PAGE_SIZE).min(self.data.len() as u64);
                let mut buffer = self.data[index(page)..index(end)].to_vec();
                self.overlay(&mut buffer, page);
                if let Some(pages) = self.modified.as_mut() {
                    pages.insert(page, buffer);
                }
            }
            let Some(buffer) = self
                .modified
                .as_mut()
                .and_then(|pages| pages.get_mut(&page))
            else {
                return;
            };
            let target = buffer.get_mut(index(offset - page)..).unwrap_or_default();
            let count = target.len().min(data.len());
            target[..count].copy_from_slice(&data[..count]);
            if count == 0 {
                return;
            }
            offset += count as u64;
            data = &data[count..];
        }
    }

    pub(super) fn pointer_at(&self, offset: u64) -> u64 {
        let page_start = offset & !(PAGE_SIZE - 1);
        if offset + 8 <= page_start + PAGE_SIZE {
            if let Some(page) = self
                .modified
                .as_ref()
                .and_then(|pages| pages.get(&page_start))
            {
                return read_u64(page, index(offset - page_start));
            }
            if self.instructions.get(&page_start).is_none_or(Vec::is_empty) {
                return read_u64(&self.data, index(offset));
            }
        }

        let mut value = [0u8; 8];
        let start = index(offset).min(self.data.len());
        let available = &self.data[start..];
        let count = available.len().min(8);
        value[..count].copy_from_slice(&available[..count]);
        self.overlay(&mut value, offset);
        u64::from_le_bytes(value)
    }

    pub(super) fn write_instruction(&mut self, offset: u64, value: u32) {
        if self.modified.is_some() {
            let instructions = std::sync::Arc::make_mut(&mut self.instructions);
            let mut page = offset & !(PAGE_SIZE - 1);
            while page < offset + 4 {
                instructions
                    .entry(page)
                    .or_default()
                    .push(InstructionPatch { offset, value });
                page += PAGE_SIZE;
            }
            return;
        }
        self.write_at(offset, &value.to_le_bytes());
    }

    /// Overlays private relocations and immutable instruction patches, clipping each page
    /// and patch to the destination, including unaligned segment and chunk boundaries.
    pub(super) fn overlay(&self, data: &mut [u8], offset: u64) {
        let end = offset + data.len() as u64;
        let mut page = offset & !(PAGE_SIZE - 1);
        while page < end {
            let first = page.max(offset);
            let last = (page + PAGE_SIZE).min(end);
            if let Some(modified) = self.modified.as_ref().and_then(|pages| pages.get(&page)) {
                let source = &modified[index(first - page)..index(last - page)];
                data[index(first - offset)..index(last - offset)].copy_from_slice(source);
            } else if let Some(patches) = self.instructions.get(&page) {
                for patch in patches {
                    let start = first.max(patch.offset);
                    let stop = last.min(patch.offset + 4);
                    if start >= stop {
                        continue;
                    }
                    let encoded = patch.value.to_le_bytes();
                    data[index(start - offset)..index(stop - offset)].copy_from_slice(
                        &encoded[index(start - patch.offset)..index(stop - patch.offset)],
                    );
                }
            }
            page += PAGE_SIZE;
        }
    }

    /// Keeps staging memory bounded regardless of image size and guest count.
    pub(super) fn write_segment(
        &self,
        memory: &mut (impl Memory + ?Sized),
        address: u64,
        start: u64,
        end: u64,
        buffer: &mut [u8],
    ) -> Result<()> {
        if !self.has_overlay() {
            return memory
                .mem_write(address, &self.data[index(start)..index(end)])
                .context("write image data");
        }

        let mut position = start;
        while position < end {
            let stop = (position + buffer.len() as u64).min(end);
            let chunk = &mut buffer[..index(stop - position)];
            chunk.copy_from_slice(&self.data[index(position)..index(stop)]);
            self.overlay(chunk, position);
            memory
                .mem_write(address + position - start, chunk)
                .context("write image chunk")?;
            position = stop;
        }
        Ok(())
    }

    /// Reads the bytes that will be loaded into the guest, so fixup parsing sees the
    /// instruction patches in mixed code/data sections.
    pub(super) fn read_at(&self, offset: u64, out: &mut [u8]) -> usize {
        if offset >= self.data.len() as u64 {
            return 0;
        }
        let available = &self.data[index(offset)..];
        let count = available.len().min(out.len());
        out[..count].copy_from_slice(&available[..count]);
        if self.has_overlay() {
            self.overlay(&mut out[..count], offset);
        }
        count
    }
}

fn read_u64(data: &[u8], start: usize) -> u64 {
    let mut value = [0u8; 8];
    if let Some(bytes) = start.checked_add(8).and_then(|end| data.get(start..end)) {
        value.copy_from_slice(bytes);
    }
    u64::from_le_bytes(value)
}
