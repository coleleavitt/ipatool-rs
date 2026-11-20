//! Go `profile_pages.go`.

use super::memory::PROFILE_PAGE_SIZE;
use super::{Error, Result};
use crate::profile::Page;

/// Go's `profileMemory`.
pub(crate) trait ProfileMemory {
    fn write(&mut self, address: u64, data: &[u8]) -> Result<()>;
}

fn page_end(data: &[u8], offset: u64) -> Result<()> {
    let length = data.len() as u64;
    if offset > length || PROFILE_PAGE_SIZE > length - offset {
        return Err(Error::msg("profile page exceeds data"));
    }
    Ok(())
}

/// Batches consecutive guest pages whose source bytes are also consecutive. Gaps remain
/// separate writes, so strict profile validation covers the same pages.
pub(crate) fn write_profile_pages(
    memory: &mut impl ProfileMemory,
    pages: &[Page],
    data: &[u8],
) -> Result<()> {
    let mut first = 0;
    while first < pages.len() {
        let page = pages[first];
        page_end(data, page.offset)?;
        let mut last = first + 1;
        while last < pages.len() {
            let (next, previous) = (pages[last], pages[last - 1]);
            if previous.address > u64::MAX - PROFILE_PAGE_SIZE
                || next.address != previous.address + PROFILE_PAGE_SIZE
                || next.offset != previous.offset + PROFILE_PAGE_SIZE
            {
                break;
            }
            page_end(data, next.offset)?;
            last += 1;
        }
        let start =
            usize::try_from(page.offset).map_err(|_| Error::msg("profile page exceeds data"))?;
        let end = start + (last - first) * 4096;
        memory
            .write(page.address, &data[start..end])
            .map_err(|error| error.wrap("write Store profile pages"))?;
        first = last;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct PageMemory {
        writes: Vec<(u64, Vec<u8>)>,
        fail: bool,
    }

    impl ProfileMemory for PageMemory {
        fn write(&mut self, address: u64, data: &[u8]) -> Result<()> {
            if self.fail {
                return Err(Error::msg("write failure"));
            }
            self.writes.push((address, data.to_vec()));
            Ok(())
        }
    }

    fn page(address: u64, offset: u64) -> Page {
        Page { address, offset }
    }

    #[test]
    fn batches_consecutive_pages_without_writing_gaps() {
        let data: Vec<u8> = (0..3 * 4096)
            .map(|n| u8::try_from(n / 4096 + 1).unwrap())
            .collect();
        let mut memory = PageMemory::default();
        write_profile_pages(
            &mut memory,
            &[page(0x1000, 0), page(0x2000, 4096), page(0x9000, 8192)],
            &data,
        )
        .unwrap();
        assert_eq!(
            memory.writes,
            [
                (0x1000, data[..8192].to_vec()),
                (0x9000, data[8192..].to_vec())
            ]
        );
    }

    #[test]
    fn preserves_writes_out_of_order() {
        let data = vec![0; 3 * 4096];
        let mut memory = PageMemory::default();
        let pages = [page(0x2000, 0), page(0x1000, 8192), page(0x2000, 4096)];
        write_profile_pages(&mut memory, &pages, &data).unwrap();
        assert_eq!(memory.writes.len(), 3);
        for (write, page) in memory.writes.iter().zip(pages) {
            assert_eq!(write.0, page.address);
            assert_eq!(write.1.len(), 4096);
        }
    }

    #[test]
    fn rejects_truncated_and_wrapping_source_ranges() {
        for offset in [1, 4096, u64::MAX - 4095] {
            let mut memory = PageMemory::default();
            let error =
                write_profile_pages(&mut memory, &[page(0x1000, offset)], &[0; 4096]).unwrap_err();
            assert_eq!(error.to_string(), "profile page exceeds data");
            assert_eq!(memory.writes, []);
        }
    }

    #[test]
    fn does_not_merge_across_a_wrapping_guest_address() {
        let mut memory = PageMemory::default();
        write_profile_pages(
            &mut memory,
            &[page(u64::MAX - 4095, 0), page(0, 4096)],
            &[0; 8192],
        )
        .unwrap();
        assert_eq!(memory.writes.len(), 2);
    }

    #[test]
    fn preserves_memory_write_errors() {
        let mut memory = PageMemory {
            fail: true,
            ..PageMemory::default()
        };
        let error = write_profile_pages(&mut memory, &[page(0x1000, 0)], &[0; 4096]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "write Store profile pages: write failure"
        );
    }
}
