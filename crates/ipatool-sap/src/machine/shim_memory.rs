//! Heap, VM and libc memory services. Port of Go `shim_memory.go`.

use std::cmp::Ordering;

use super::shims::{
    Abi, FreeBlock, Guest, GuestAllocation, Handler, MAX_GUEST_TRANSFER, Service, State, Uc,
    add_aliases, checked_size, handler,
};
use super::{HEAP_BASE, HEAP_SIZE, PAGE_SIZE, align};
use crate::error::{Context, Error, Result};

pub(crate) fn register(uc: &mut Uc) -> Result<()> {
    let services: [(&[&str], Service); 25] = [
        (&["_malloc", "_IOMalloc", "_IOMallocData"], malloc),
        (&["_IOMallocAligned"], malloc_aligned),
        (&["_IOMallocTypeImpl"], malloc_typed),
        (
            &["_IOMallocTypeVarImpl", "_OSObject_typed_operator_new"],
            malloc_typed_variable,
        ),
        (&["_IOLockAlloc", "_IORWLockAlloc"], malloc_lock),
        (&["_malloc_good_size"], malloc_good_size),
        (&["_malloc_size"], malloc_size),
        (&["_calloc"], calloc),
        (&["_realloc", "_reallocf"], realloc),
        (
            &[
                "_free",
                "_IOFree",
                "_IOFreeAligned",
                "_IOFreeData",
                "_IOLockFree",
                "_IORWLockFree",
                "_OSObject_typed_operator_delete",
            ],
            free,
        ),
        (&["_IOFreeTypeImpl", "_IOFreeTypeVarImpl"], free_typed),
        (&["_mmap"], mmap),
        (&["_munmap"], munmap),
        (&["_vm_allocate"], vm_allocate),
        (&["_vm_deallocate"], vm_deallocate),
        (&["_memcpy", "_memmove"], memmove),
        (&["_memset"], memset),
        (&["___bzero"], bzero),
        (&["___memcpy_chk"], checked_memcpy),
        (&["___memset_chk"], checked_memset),
        (&["_memcmp"], memcmp),
        (&["_strcmp"], strcmp),
        (&["_strncmp"], strncmp),
        (&["_strlen"], strlen),
        (&["_strnlen"], strnlen),
    ];
    for (names, function) in services {
        let service: Handler = handler(function);
        add_aliases(uc, names, &service).context("clear allocated guest memory")?;
    }
    Ok(())
}

pub(crate) fn malloc(uc: &mut Uc) -> Result<()> {
    let size = uc.argument(0).context("write reallocated guest memory")?;
    let address = uc.get_data_mut().allocate(size)?;
    uc.set_result(address)
}

fn malloc_aligned(uc: &mut Uc) -> Result<()> {
    let size = uc.argument(0)?;
    let alignment = uc.argument(1)?;
    if alignment == 0 || alignment & (alignment - 1) != 0 {
        return Err(Error::msg(format!(
            "invalid allocation alignment {alignment:#x}"
        )));
    }
    let address = uc.get_data_mut().allocate_aligned(size, alignment)?;
    uc.set_result(address)
}

fn malloc_typed(uc: &mut Uc) -> Result<()> {
    let view = uc.argument(0)?;
    let size = uc
        .read_u32(view + 44)
        .context("read typed allocation size")?;
    let address = uc.get_data_mut().allocate(u64::from(size & 0x00ff_ffff))?;
    uc.set_result(address)
}

fn malloc_typed_variable(uc: &mut Uc) -> Result<()> {
    let size = uc.argument(1)?;
    let address = uc.get_data_mut().allocate(size)?;
    uc.set_result(address)
}

fn malloc_lock(uc: &mut Uc) -> Result<()> {
    let address = uc.get_data_mut().allocate(8)?;
    uc.write_mem(address, &[0; 8])
        .context("initialize malloc lock")?;
    uc.set_result(address)
}

fn free_typed(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(1)?;
    if address != 0 {
        release(uc, address)?;
    }
    uc.set_result(0)
}

fn malloc_good_size(uc: &mut Uc) -> Result<()> {
    let size = uc.argument(0)?;
    uc.set_result(align(size.max(1), 16))
}

fn malloc_size(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(0)?;
    let reserved = uc
        .get_data()
        .allocations
        .get(&address)
        .map_or(0, |allocation| allocation.reserved);
    uc.set_result(reserved)
}

fn calloc(uc: &mut Uc) -> Result<()> {
    let count = uc.argument(0)?;
    let size = uc.argument(1)?;
    let Some(total) = count.checked_mul(size) else {
        return Err(Error::msg("allocation size overflows"));
    };
    let address = uc.get_data_mut().allocate(total)?;
    if total != 0
        && let Err(error) = uc.write_mem(address, &zeroes(total)?)
    {
        let _ = release(uc, address);
        return Err(error.wrap("clear allocated guest memory"));
    }
    uc.set_result(address)
}

fn realloc(uc: &mut Uc) -> Result<()> {
    let old_address = uc.argument(0).context("write reallocated guest memory")?;
    let new_size = uc.argument(1)?;
    if old_address == 0 {
        let address = uc.get_data_mut().allocate(new_size)?;
        return uc.set_result(address);
    }
    let Some(old) = uc.get_data().allocations.get(&old_address).copied() else {
        return Err(Error::msg(format!(
            "reallocate unknown pointer {old_address:#x}"
        )));
    };
    if new_size <= old.reserved {
        uc.get_data_mut().allocations.insert(
            old_address,
            GuestAllocation {
                size: new_size,
                reserved: old.reserved,
            },
        );
        return uc.set_result(old_address);
    }
    let new_address = uc.get_data_mut().allocate(new_size)?;
    let data = match uc.read_mem(old_address, old.size) {
        Ok(data) => data,
        Err(error) => {
            let _ = release(uc, new_address);
            return Err(error.wrap("read reallocated guest memory"));
        }
    };
    if let Err(error) = uc.write_mem(new_address, &data) {
        let _ = release(uc, new_address);
        return Err(error.wrap("write reallocated guest memory"));
    }
    release(uc, old_address)?;
    uc.set_result(new_address)
}

pub(crate) fn free(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(0)?;
    if address != 0 {
        release(uc, address)?;
    }
    uc.set_result(0)
}

impl State {
    pub(crate) fn allocate(&mut self, size: u64) -> Result<u64> {
        if size > MAX_GUEST_TRANSFER {
            return Err(Error::msg(format!("allocation size {size} exceeds limit")));
        }
        let reserved = align(size.max(1), 16);
        if let Some(index) = self
            .free_blocks
            .iter()
            .position(|block| block.size >= reserved)
        {
            let address = self.free_blocks[index].address;
            if self.free_blocks[index].size == reserved {
                self.free_blocks.remove(index);
            } else {
                self.free_blocks[index].address += reserved;
                self.free_blocks[index].size -= reserved;
            }
            self.allocations
                .insert(address, GuestAllocation { size, reserved });
            return Ok(address);
        }
        if self.heap_cursor > HEAP_SIZE || reserved > HEAP_SIZE - self.heap_cursor {
            return Err(Error::msg("guest heap exhausted"));
        }
        let address = HEAP_BASE + self.heap_cursor;
        self.heap_cursor += reserved;
        self.allocations
            .insert(address, GuestAllocation { size, reserved });
        Ok(address)
    }

    pub(crate) fn allocate_aligned(&mut self, size: u64, alignment: u64) -> Result<u64> {
        if size > MAX_GUEST_TRANSFER {
            return Err(Error::msg(format!("allocation size {size} exceeds limit")));
        }
        let reserved = align(size.max(1), alignment);
        let cursor = align(self.heap_cursor, alignment);
        if cursor > HEAP_SIZE || reserved > HEAP_SIZE - cursor {
            return Err(Error::msg("guest heap exhausted"));
        }
        let address = HEAP_BASE + cursor;
        self.heap_cursor = cursor + reserved;
        self.allocations
            .insert(address, GuestAllocation { size, reserved });
        Ok(address)
    }

    fn coalesce_free_blocks(&mut self) {
        self.free_blocks.sort_by_key(|block| block.address);
        let mut merged: Vec<FreeBlock> = Vec::with_capacity(self.free_blocks.len());
        for block in self.free_blocks.drain(..) {
            if let Some(last) = merged.last_mut()
                && last.address + last.size == block.address
            {
                last.size += block.size;
                continue;
            }
            merged.push(block);
        }
        self.free_blocks = merged;
        while let Some(&block) = self.free_blocks.last() {
            if block.address + block.size != HEAP_BASE + self.heap_cursor {
                break;
            }
            self.heap_cursor -= block.size;
            self.free_blocks.pop();
        }
    }
}

pub(crate) fn release(uc: &mut Uc, address: u64) -> Result<()> {
    let Some(allocation) = uc.get_data().allocations.get(&address).copied() else {
        return Err(Error::msg(format!("free unknown pointer {address:#x}")));
    };
    uc.write_mem(address, &zeroes(allocation.reserved)?)
        .context("clear released guest memory")?;
    let state = uc.get_data_mut();
    state.allocations.remove(&address);
    state.free_blocks.push(FreeBlock {
        address,
        size: allocation.reserved,
    });
    state.coalesce_free_blocks();
    Ok(())
}

pub(crate) fn zeroes(size: u64) -> Result<Vec<u8>> {
    let size = usize::try_from(size)
        .map_err(|_| Error::msg(format!("guest buffer size {size} exceeds limit")))?;
    Ok(vec![0; size])
}

fn mmap(uc: &mut Uc) -> Result<()> {
    let length = uc.argument(1)?;
    let Ok(address) = uc.get_data_mut().allocate_aligned(length, PAGE_SIZE) else {
        return super::shim_platform::return_minus_one(uc);
    };
    if let Err(error) = uc.write_mem(address, &zeroes(length)?) {
        let _ = release(uc, address);
        return Err(error.wrap("clear mapped guest memory"));
    }
    uc.set_result(address)
}

fn munmap(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(0)?;
    if release(uc, address).is_err() {
        return uc.set_result(u64::MAX);
    }
    uc.set_result(0)
}

fn vm_allocate(uc: &mut Uc) -> Result<()> {
    let field = uc.argument(1)?;
    let size = uc.argument(2)?;
    let Ok(address) = uc.get_data_mut().allocate_aligned(size, PAGE_SIZE) else {
        return uc.set_result(3); // KERN_NO_SPACE
    };
    if let Err(error) = uc.write_mem(address, &zeroes(size)?) {
        let _ = release(uc, address);
        return Err(error.wrap("clear allocated VM memory"));
    }
    if let Err(error) = uc.write_u64(field, address) {
        let _ = release(uc, address);
        return Err(error);
    }
    uc.set_result(0)
}

fn vm_deallocate(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(1)?;
    if release(uc, address).is_err() {
        return uc.set_result(1); // KERN_INVALID_ADDRESS
    }
    uc.set_result(0)
}

fn memmove(uc: &mut Uc) -> Result<()> {
    let destination = uc.argument(0)?;
    let source = uc.argument(1)?;
    let length = uc.argument(2)?;
    checked_size(length)?;
    let data = uc.read_mem(source, length).context("read memmove source")?;
    uc.write_mem(destination, &data)
        .context("write memmove destination")?;
    uc.set_result(destination)
}

fn memset(uc: &mut Uc) -> Result<()> {
    let destination = uc.argument(0)?;
    let value = uc.argument(1)?;
    let length = uc.argument(2)?;
    let size = checked_size(length)?;
    uc.write_mem(destination, &vec![value.to_le_bytes()[0]; size])
        .context("write guest memory fill")?;
    uc.set_result(destination)
}

fn bzero(uc: &mut Uc) -> Result<()> {
    let destination = uc.argument(0)?;
    let length = uc.argument(1)?;
    let size = checked_size(length)?;
    uc.write_mem(destination, &vec![0; size])
        .context("clear guest memory")?;
    uc.set_result(destination)
}

fn checked_memcpy(uc: &mut Uc) -> Result<()> {
    let length = uc.argument(2)?;
    let capacity = uc.argument(3)?;
    if length > capacity {
        return Err(Error::msg("checked copy exceeds destination"));
    }
    memmove(uc)
}

fn checked_memset(uc: &mut Uc) -> Result<()> {
    let length = uc.argument(2)?;
    let capacity = uc.argument(3)?;
    if length > capacity {
        return Err(Error::msg("checked fill exceeds destination"));
    }
    memset(uc)
}

fn ordering(value: Ordering) -> u64 {
    (value as i64).cast_unsigned()
}

fn memcmp(uc: &mut Uc) -> Result<()> {
    let left = uc.argument(0)?;
    let right = uc.argument(1)?;
    let length = uc.argument(2)?;
    checked_size(length)?;
    let a = uc
        .read_mem(left, length)
        .context("read left memory operand")?;
    let b = uc
        .read_mem(right, length)
        .context("read right memory operand")?;
    uc.set_result(ordering(a.cmp(&b)))
}

fn strcmp(uc: &mut Uc) -> Result<()> {
    let left = uc.argument(0)?;
    let right = uc.argument(1)?;
    let a = uc.read_c_string(left)?;
    let b = uc.read_c_string(right)?;
    uc.set_result(ordering(a.cmp(&b)))
}

fn strncmp(uc: &mut Uc) -> Result<()> {
    let left = uc.argument(0)?;
    let right = uc.argument(1)?;
    let length = uc.argument(2)?;
    checked_size(length)?;
    let mut offset = 0;
    while offset < length {
        if left > u64::MAX - offset || right > u64::MAX - offset {
            return Err(Error::msg("string comparison address overflows"));
        }
        let left_address = left + offset;
        let right_address = right + offset;
        let chunk = (length - offset)
            .min(PAGE_SIZE - left_address % PAGE_SIZE)
            .min(PAGE_SIZE - right_address % PAGE_SIZE);
        let a = uc
            .read_mem(left_address, chunk)
            .context("read left memory page")?;
        let b = uc
            .read_mem(right_address, chunk)
            .context("read right memory page")?;
        for (&x, &y) in a.iter().zip(&b) {
            if x != y {
                return uc.set_result((i64::from(x) - i64::from(y)).cast_unsigned());
            }
            if x == 0 {
                return uc.set_result(0);
            }
        }
        offset += chunk;
    }
    uc.set_result(0)
}

fn strlen(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(0)?;
    let value = uc.read_c_string(address)?;
    uc.set_result(value.len() as u64)
}

fn strnlen(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(0)?;
    let limit = uc.argument(1)?;
    if limit > MAX_GUEST_TRANSFER {
        return Err(Error::msg(format!(
            "string length limit {limit} exceeds maximum"
        )));
    }
    let mut offset = 0;
    while offset < limit {
        let chunk = (limit - offset).min(PAGE_SIZE - (address + offset) % PAGE_SIZE);
        let data = uc
            .read_mem(address + offset, chunk)
            .context("read bounded guest string")?;
        if let Some(index) = data.iter().position(|&byte| byte == 0) {
            return uc.set_result(offset + index as u64);
        }
        offset += chunk;
    }
    uc.set_result(limit)
}
