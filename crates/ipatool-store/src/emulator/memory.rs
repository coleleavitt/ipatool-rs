//! Go `memory.go`: grouped profile mappings and the strict profile-page check.

use std::collections::HashSet;

use super::{Error, Result};

pub(crate) const PROFILE_PAGE_SIZE: u64 = 4096;
const MAX_REGION_GAP: u64 = 1 << 20;
const MAX_REGION_SIZE: u64 = 32 << 20;
const MAX_PROFILE_SPAN: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemoryRegion {
    pub(crate) address: u64,
    pub(crate) size: u64,
}

/// Groups nearby pages to avoid rebuilding Unicorn's memory topology thousands of times during
/// both mapping and teardown. Individual and total spans are bounded so a chain of small gaps
/// cannot silently reserve an arbitrarily large address range.
pub(crate) fn group_pages(addresses: &mut [u64]) -> Result<Vec<MemoryRegion>> {
    addresses.sort_unstable();
    if addresses
        .iter()
        .any(|&address| address % PROFILE_PAGE_SIZE != 0 || address > u64::MAX - PROFILE_PAGE_SIZE)
    {
        return Err(Error::msg("invalid Store profile page address"));
    }
    let mut regions = Vec::new();
    let mut total = 0u64;
    let mut start = 0;
    while start < addresses.len() {
        let mut end = start + 1;
        while end < addresses.len()
            && addresses[end] - addresses[end - 1] <= MAX_REGION_GAP
            && addresses[end] - addresses[start] + PROFILE_PAGE_SIZE <= MAX_REGION_SIZE
        {
            end += 1;
        }
        let size = addresses[end - 1] - addresses[start] + PROFILE_PAGE_SIZE;
        if size > MAX_PROFILE_SPAN - total {
            return Err(Error::msg("store profile mapping exceeds size limit"));
        }
        total += size;
        regions.push(MemoryRegion {
            address: addresses[start],
            size,
        });
        start = end;
    }
    Ok(regions)
}

/// Checks every touched page, including accesses that start in a valid page and cross a gap.
/// Grouping padding is never a valid dependency.
pub(crate) fn check_profile_access(allowed: &HashSet<u64>, address: u64, size: u64) -> Result<()> {
    if size == 0 {
        return Ok(());
    }
    if size - 1 > u64::MAX - address {
        return Err(Error::msg("store memory access overflows address space"));
    }
    let last = (address + (size - 1)) & !(PROFILE_PAGE_SIZE - 1);
    let mut page = address & !(PROFILE_PAGE_SIZE - 1);
    loop {
        if !allowed.contains(&page) {
            return Err(Error::msg(format!(
                "missing Store profile page {page:#x} for access {address:#x} ({size} bytes)"
            )));
        }
        if page == last {
            return Ok(());
        }
        page += PROFILE_PAGE_SIZE;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_nearby_unsorted_pages_and_tolerates_duplicates() {
        let regions = group_pages(&mut [0x3000, 0x1000, 0x1000, 0x80_0000]).unwrap();
        assert_eq!(
            regions,
            [
                MemoryRegion {
                    address: 0x1000,
                    size: 0x3000
                },
                MemoryRegion {
                    address: 0x80_0000,
                    size: 4096
                }
            ]
        );
    }

    #[test]
    fn bounds_groups_in_a_long_chain_of_nearby_pages() {
        let pages: Vec<u64> = (0..3 * MAX_REGION_SIZE)
            .step_by(usize::try_from(MAX_REGION_GAP).unwrap())
            .collect();
        let regions = group_pages(&mut pages.clone()).unwrap();
        assert!(regions.iter().all(|region| region.size <= MAX_REGION_SIZE));
        for address in pages {
            assert!(regions.iter().any(|region| address >= region.address
                && address + PROFILE_PAGE_SIZE <= region.address + region.size));
        }
    }

    #[test]
    fn rejects_excessive_total_mapped_memory() {
        let mut pages: Vec<u64> = (0..2 * MAX_PROFILE_SPAN)
            .step_by(usize::try_from(MAX_REGION_GAP).unwrap())
            .collect();
        let error = group_pages(&mut pages).unwrap_err();
        assert!(error.to_string().contains("size limit"));
    }

    #[test]
    fn rejects_unaligned_and_overflowing_pages() {
        for address in [1, u64::MAX - 4095] {
            let error = group_pages(&mut [address]).unwrap_err();
            assert!(error.to_string().contains("page address"));
        }
    }

    #[test]
    fn checks_the_complete_range() {
        let allowed = HashSet::from([0x1000, 0x3000]);
        for (address, size) in [(0x1ffc, 8), (0x1000, 0x2001)] {
            let error = check_profile_access(&allowed, address, size).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("missing Store profile page 0x2000")
            );
        }
        check_profile_access(&allowed, 0x1000, 4096).unwrap();
        check_profile_access(&allowed, 0x3000, 4096).unwrap();
    }

    #[test]
    fn handles_empty_accesses_and_rejects_wrapping_ranges() {
        check_profile_access(&HashSet::new(), u64::MAX, 0).unwrap();
        let error = check_profile_access(&HashSet::new(), u64::MAX, 2).unwrap_err();
        assert!(error.to_string().contains("overflows"));
        check_profile_access(&HashSet::from([!4095]), u64::MAX, 1).unwrap();
    }
}
