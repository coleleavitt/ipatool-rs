//! Port of Go `profilegen/vm.go`: the pinned Swift runtime and Store images run under
//! Unicorn, with the few host services the bootstrap needs answered here.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use ipatool_sap::engine::{self, unicorn_error};
use ipatool_sap::machimage::without_pointer_authentication;
use ipatool_store::profile::{Profile, witness_key};
use unicorn_engine::{HookType, Prot, RegisterARM64, Unicorn};

use crate::error::{Error, Result};
use crate::objc::class_name;
use crate::source::{Source, inside, le64, to_usize};
use crate::swift_metadata::new_metadata_profile;

pub(crate) const BOOT_HEAP: u64 = 0x300_0000_0000;
pub(crate) const BOOT_STACK: u64 = 0x310_0000_0000;
pub(crate) const BOOT_RETURN: u64 = 0x1_0000_0000;
pub(crate) const BOOT_HEAP_SIZE: u64 = 64 << 20;
const BOOT_STACK_SIZE: u64 = 8 << 20;
const NOP: u32 = 0xd503_201f;
const RET: u32 = 0xd65f_03c0;
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_INSTRUCTION_LIMIT: usize = 20_000_000;
const RECENT_SYMBOLS: usize = 16;

pub(crate) type Vm<'u, 'c> = Unicorn<'u, State<'c>>;

/// Go's `bootstrapVM` fields other than the engine.
pub(crate) struct State<'c> {
    pub(crate) metadata: Profile,
    metadata_returns: HashMap<u64, Vec<MetadataReturn>>,
    atomics: HashMap<u64, u32>,
    pub(crate) source: Source<'c>,
    pub(crate) mapped: HashSet<u64>,
    pub(crate) cursor: u64,
    main_header: u64,
    pub(crate) allocations: HashMap<u64, u64>,
    tls: HashMap<u64, u64>,
    callbacks: HashMap<u64, Callback>,
    fault: Option<String>,
    recent: Vec<String>,
}

/// A guest call made by a host service; returning to `resume` restores `link` and runs
/// `after`, Go's callback closures.
struct Callback {
    link: u64,
    after: After,
}

pub(crate) enum After {
    /// `swift_once`/`dispatch_once_f` finished: mark the token done.
    Once { token: u64 },
    /// An image-load callback returned: report the next image.
    NextImage { function: u64, index: usize },
}

struct MetadataReturn {
    stack: u64,
    name: &'static str,
    key: String,
    alias: String,
}

impl<'c> State<'c> {
    pub(crate) fn new(source: Source<'c>) -> Self {
        Self {
            metadata: new_metadata_profile(),
            metadata_returns: HashMap::new(),
            atomics: HashMap::new(),
            source,
            mapped: HashSet::new(),
            cursor: 0,
            main_header: 0,
            allocations: HashMap::new(),
            tls: HashMap::new(),
            callbacks: HashMap::new(),
            fault: None,
            recent: Vec::new(),
        }
    }

    /// Rewrites the instructions of a page Unicorn cannot run; atomics are emulated in
    /// `step` and their words become NOPs.
    fn lower_code(&mut self, page: u64, data: &mut [u8]) {
        for offset in (0..4096).step_by(4) {
            if !inside(page + offset, &self.source.code) {
                continue;
            }
            let index = to_usize(offset);
            let mut word = crate::source::le32(&data[index..]);
            if is_bootstrap_atomic(word) {
                self.atomics.insert(page + offset, word);
                word = NOP;
            }
            data[index..index + 4].copy_from_slice(&bootstrap_instruction(word).to_le_bytes());
        }
    }

    /// Go's `alloc`: a 16-byte aligned bump allocation in the guest heap.
    pub(crate) fn alloc(&mut self, size: u64) -> Result<u64> {
        if size > BOOT_HEAP_SIZE || self.cursor > BOOT_HEAP_SIZE {
            return Err(Error::panic("bootstrap heap exhausted"));
        }
        let size = match (size + 15) & !15 {
            0 => 16,
            size => size,
        };
        if size > BOOT_HEAP_SIZE - self.cursor {
            return Err(Error::panic("bootstrap heap exhausted"));
        }
        let address = BOOT_HEAP + self.cursor;
        self.cursor += size;
        self.allocations.insert(address, size);
        Ok(address)
    }
}

/// Go's `newBootstrapVM`.
pub(crate) fn new_vm(source: Source<'_>) -> Result<Vm<'_, '_>> {
    let create = |error: ipatool_sap::Error| Error::failed(format!("create bootstrap VM: {error}"));
    let mut vm = engine::new(State::new(source)).map_err(create)?;
    for (address, size) in [
        (BOOT_HEAP, BOOT_HEAP_SIZE),
        (BOOT_STACK, BOOT_STACK_SIZE),
        (BOOT_RETURN, 4096),
    ] {
        vm.mem_map(address, size, Prot::ALL)
            .map_err(|error| create(unicorn_error(error)))?;
    }
    vm.add_mem_hook(HookType::MEM_UNMAPPED, 1, 0, |vm, _, address, size, _| {
        page_fault(vm, address, u32::try_from(size).unwrap_or(u32::MAX))
    })
    .map_err(|error| create(unicorn_error(error)))?;
    vm.add_code_hook(1, 0, |vm, pc, _| step(vm, pc))
        .map_err(|error| create(unicorn_error(error)))?;
    if let Some(&environ) = vm.get_data().source.exports.get("_environ")
        && environ != 0
    {
        let value = vm.get_data_mut().alloc(8)?;
        vm.put(environ, value)?;
    }
    Ok(vm)
}

/// Go's `pageFault`: maps cache pages on demand, lowering instructions Unicorn lacks.
fn page_fault(vm: &mut Vm<'_, '_>, address: u64, size: u32) -> bool {
    match map_pages(vm, address, size) {
        Ok(()) => true,
        Err(error) => {
            let fault = fault_report(vm, address, size, &error);
            vm.get_data_mut().fault = Some(fault);
            false
        }
    }
}

fn map_pages(vm: &mut Vm<'_, '_>, address: u64, size: u32) -> Result<()> {
    if size > 1 << 20 {
        return Err(Error::panic("oversized guest access"));
    }
    let end = (address + u64::from(size) + 4095) & !4095;
    let mut page = address & !4095;
    while page < end {
        if !vm.get_data().mapped.contains(&page) {
            let mut data = vm.get_data().source.read(page, 4096)?;
            vm.get_data_mut().lower_code(page, &mut data);
            vm.mem_map(page, 4096, Prot::ALL)
                .map_err(|error| Error::panic(unicorn_error(error)))?;
            vm.mem_write(page, &data)
                .map_err(|error| Error::panic(unicorn_error(error)))?;
            vm.get_data_mut().mapped.insert(page);
        }
        page += 4096;
    }
    Ok(())
}

fn fault_report(vm: &Vm<'_, '_>, address: u64, size: u32, error: &Error) -> String {
    let pc = vm.reg_read(RegisterARM64::PC).unwrap_or_default();
    let code = vm
        .mem_read_as_vec(pc.wrapping_sub(16), 48)
        .unwrap_or_default();
    let registers: Vec<String> = (0..29)
        .map(|index| format!("{:x}", vm.reg_read(x(index)).unwrap_or_default()))
        .collect();
    let link = vm.reg_read(RegisterARM64::LR).unwrap_or_default();
    let state = vm.get_data();
    format!(
        "bootstrap memory at {pc:#x} ({}), address {address:#x} size {size}: {error}; code={} regs=[{}] lr={link:#x}; recent {}",
        state.source.describe(pc),
        crate::hex(&code),
        registers.join(" "),
        go_list(&state.recent)
    )
}

/// Go's code hook `step`.
fn step(vm: &mut Vm<'_, '_>, pc: u64) {
    if let Err(error) = try_step(vm, pc) {
        let state = vm.get_data_mut();
        state.fault = Some(format!(
            "bootstrap at {pc:#x}: {error}; recent {}",
            go_list(&state.recent)
        ));
        let _stopped = vm.emu_stop();
    }
}

fn try_step(vm: &mut Vm<'_, '_>, pc: u64) -> Result<()> {
    vm.observe_metadata_return(pc)?;
    if let Some(&word) = vm.get_data().atomics.get(&pc) {
        vm.atomic(word)?;
        return vm.write_register(RegisterARM64::PC, pc + 4);
    }
    if let Some(callback) = vm.get_data_mut().callbacks.remove(&pc) {
        vm.write_register(RegisterARM64::LR, callback.link)?;
        vm.write_register(RegisterARM64::PC, callback.link)?;
        return match callback.after {
            After::Once { token } => vm.put(token, u64::MAX),
            After::NextImage { function, index } => vm.next_image(function, index),
        };
    }
    // Shared-cache branch islands are outside individual Mach-O sections.
    if !inside(pc, &vm.get_data().source.code) {
        let raw = vm
            .mem_read_as_vec(pc, 4)
            .map_err(|error| Error::panic(unicorn_error(error)))?;
        let word = crate::source::le32(&raw);
        let replacement = without_pointer_authentication(word);
        if replacement != word {
            let target = match replacement {
                NOP => pc + 4,
                RET => vm.register(RegisterARM64::LR)?,
                _ if replacement & 0xffff_fc1f == 0xd61f_0000 => vm.arg(replacement >> 5 & 31)?,
                _ if replacement & 0xffff_fc1f == 0xd63f_0000 => {
                    let target = vm.arg(replacement >> 5 & 31)?;
                    vm.write_register(RegisterARM64::LR, pc + 4)?;
                    target
                }
                _ => return Err(Error::panic("unsupported PAC replacement")),
            };
            return vm.write_register(RegisterARM64::PC, target);
        }
    }

    let Some(name) = vm
        .get_data()
        .source
        .symbols
        .borrow()
        .get(&pc)
        .filter(|name| !name.is_empty())
        .cloned()
    else {
        return Ok(());
    };
    let recent = &mut vm.get_data_mut().recent;
    if recent.last() != Some(&name) {
        recent.push(name.clone());
        if recent.len() > RECENT_SYMBOLS {
            recent.remove(0);
        }
    }
    vm.observe_metadata_call(&name)?;
    if vm.host(&name)? {
        let link = vm.register(RegisterARM64::LR)?;
        vm.write_register(RegisterARM64::PC, link)?;
    }
    Ok(())
}

/// Go's `bootstrapInstruction`. Bootstrap is single-threaded: `RCpc` acquire loads have
/// ordinary load semantics, and Unicorn 2.1 does not decode LDAPR even with its maximum
/// ARM64 CPU model.
pub(crate) const fn bootstrap_instruction(word: u32) -> u32 {
    match word & 0xffff_fc00 {
        0xf8bf_c000 => 0xf940_0000 | word & 0x3ff,
        0xb8bf_c000 => 0xb940_0000 | word & 0x3ff,
        0x78bf_c000 => 0x7940_0000 | word & 0x3ff,
        0x38bf_c000 => 0x3940_0000 | word & 0x3ff,
        _ => without_pointer_authentication(word),
    }
}

pub(crate) const fn is_bootstrap_atomic(word: u32) -> bool {
    word & 0x3f20_0c00 == 0x3820_0000 && (word >> 12) & 15 <= 8
        || word & 0x3fa0_7c00 == 0x08a0_7c00
        || word & 0xbfa0_7c00 == 0x0820_7c00
}

/// `X<index>` for the contiguous Unicorn register numbers `X0`..`X28`.
fn x(index: u32) -> i32 {
    RegisterARM64::X0 as i32 + index.cast_signed()
}

/// Go's `%v` of a string slice.
fn go_list(items: &[String]) -> String {
    format!("[{}]", items.join(" "))
}

/// Go's `bootstrapVM` methods over the engine and its state.
pub(crate) trait Guest {
    fn register(&self, register: RegisterARM64) -> Result<u64>;
    fn write_register(&mut self, register: RegisterARM64, value: u64) -> Result<()>;
    fn arg(&self, index: u32) -> Result<u64>;
    fn set(&mut self, index: u32, value: u64) -> Result<()>;
    fn read(&mut self, address: u64, size: u64) -> Result<Vec<u8>>;
    fn write(&mut self, address: u64, data: &[u8]) -> Result<()>;
    fn u64(&mut self, address: u64) -> Result<u64>;
    fn put(&mut self, address: u64, value: u64) -> Result<()>;
    fn alloc(&mut self, size: u64) -> Result<u64>;
    fn string(&mut self, address: u64) -> Result<Vec<u8>>;
    fn cstring(&mut self, text: &str) -> Result<u64>;
    fn call(&mut self, name: &str, args: &[u64]) -> Result<(u64, u64)>;
    fn call_address(&mut self, entry: u64, args: &[u64]) -> Result<(u64, u64)>;
    fn invoke(&mut self, function: u64, args: &[u64], after: After) -> Result<()>;
    fn next_image(&mut self, function: u64, index: usize) -> Result<()>;
    fn host(&mut self, name: &str) -> Result<bool>;
    fn reg(&self, number: u32) -> Result<u64>;
    fn set_reg(&mut self, number: u32, value: u64) -> Result<()>;
    fn atomic(&mut self, word: u32) -> Result<()>;
    fn compare_exchange(&mut self, word: u32) -> Result<()>;
    fn observe_metadata_call(&mut self, name: &str) -> Result<()>;
    fn observe_metadata_return(&mut self, pc: u64) -> Result<()>;
}

impl Guest for Vm<'_, '_> {
    fn register(&self, register: RegisterARM64) -> Result<u64> {
        self.reg_read(register)
            .map_err(|error| Error::panic(unicorn_error(error)))
    }

    fn write_register(&mut self, register: RegisterARM64, value: u64) -> Result<()> {
        self.reg_write(register, value)
            .map_err(|error| Error::panic(unicorn_error(error)))
    }

    fn arg(&self, index: u32) -> Result<u64> {
        self.reg_read(x(index))
            .map_err(|error| Error::panic(unicorn_error(error)))
    }

    fn set(&mut self, index: u32, value: u64) -> Result<()> {
        self.reg_write(x(index), value)
            .map_err(|error| Error::panic(unicorn_error(error)))
    }

    fn read(&mut self, address: u64, size: u64) -> Result<Vec<u8>> {
        if size > BOOT_HEAP_SIZE || address > u64::MAX - size - 4095 {
            return Err(Error::panic("read too large"));
        }
        let end = (address + size + 4095) & !4095;
        let mut page = address & !4095;
        while page < end {
            let reserved = (BOOT_HEAP..BOOT_HEAP + BOOT_HEAP_SIZE).contains(&page)
                || (BOOT_STACK..BOOT_STACK + BOOT_STACK_SIZE).contains(&page);
            if !reserved && !self.get_data().mapped.contains(&page) && !page_fault(self, page, 4096)
            {
                return Err(Error::panic(
                    self.get_data().fault.clone().unwrap_or_default(),
                ));
            }
            page += 4096;
        }
        self.mem_read_as_vec(address, to_usize(size))
            .map_err(|error| Error::panic(unicorn_error(error)))
    }

    fn write(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.read(address, data.len() as u64)?;
        self.mem_write(address, data)
            .map_err(|error| Error::panic(unicorn_error(error)))
    }

    fn u64(&mut self, address: u64) -> Result<u64> {
        Ok(le64(&self.read(address, 8)?))
    }

    fn put(&mut self, address: u64, value: u64) -> Result<()> {
        self.write(address, &value.to_le_bytes())
    }

    fn alloc(&mut self, size: u64) -> Result<u64> {
        self.get_data_mut().alloc(size)
    }

    fn string(&mut self, mut address: u64) -> Result<Vec<u8>> {
        if address == 0 {
            return Ok(Vec::new());
        }
        let mut text = Vec::new();
        while text.len() < 1 << 20 {
            let byte = self.read(address, 1)?[0];
            if byte == 0 {
                return Ok(text);
            }
            text.push(byte);
            address += 1;
        }
        Err(Error::panic("bootstrap string too long"))
    }

    fn cstring(&mut self, text: &str) -> Result<u64> {
        let address = self.alloc(text.len() as u64 + 1)?;
        let mut data = text.as_bytes().to_vec();
        data.push(0);
        self.write(address, &data)?;
        Ok(address)
    }

    fn call(&mut self, name: &str, args: &[u64]) -> Result<(u64, u64)> {
        let entry = self
            .get_data()
            .source
            .exports
            .get(&format!("_{name}"))
            .copied()
            .unwrap_or_default();
        if entry == 0 {
            return Err(Error::failed(format!("bootstrap export {name} is missing")));
        }
        self.call_address(entry, args)
    }

    fn call_address(&mut self, entry: u64, args: &[u64]) -> Result<(u64, u64)> {
        let name = self.get_data().source.describe(entry);
        for index in 0..8 {
            self.set(index, args.get(index as usize).copied().unwrap_or_default())?;
        }
        self.write_register(RegisterARM64::SP, BOOT_STACK + BOOT_STACK_SIZE - 4096)?;
        self.write_register(RegisterARM64::LR, BOOT_RETURN)?;
        let result = engine::start_bounded(
            self,
            entry,
            BOOT_RETURN,
            CALL_TIMEOUT,
            CALL_INSTRUCTION_LIMIT,
        );
        if let Some(fault) = &self.get_data().fault {
            return Err(Error::failed(fault.clone()));
        }
        let pc = self.reg_read(RegisterARM64::PC).unwrap_or_default();
        if result.is_err() || pc != BOOT_RETURN {
            let error = result
                .err()
                .map_or_else(|| "<nil>".to_owned(), |error| error.to_string());
            let bytes = self.read(pc, 16)?;
            let state = self.get_data();
            return Err(Error::failed(format!(
                "bootstrap {name} stopped at {pc:#x}: {error} bytes={} loc={}; recent {}",
                crate::hex(&bytes),
                state.source.describe(pc),
                go_list(&state.recent)
            )));
        }
        self.observe_metadata_return(BOOT_RETURN)?;
        Ok((self.arg(0)?, self.arg(1)?))
    }

    fn invoke(&mut self, function: u64, args: &[u64], after: After) -> Result<()> {
        let link = self.register(RegisterARM64::LR)?;
        let resume = self.alloc(16)?;
        self.write(resume, &RET.to_le_bytes())?;
        self.get_data_mut()
            .callbacks
            .insert(resume, Callback { link, after });
        for (index, value) in (0..).zip(args) {
            self.set(index, *value)?;
        }
        self.write_register(RegisterARM64::LR, resume)?;
        self.write_register(RegisterARM64::PC, function)
    }

    fn next_image(&mut self, function: u64, index: usize) -> Result<()> {
        let Some(base) = self
            .get_data()
            .source
            .images
            .values()
            .nth(index)
            .map(ipatool_sap::assets::StoreImage::base_address)
        else {
            return self.set(0, 0);
        };
        self.invoke(
            function,
            &[base, 0],
            After::NextImage {
                function,
                index: index + 1,
            },
        )
    }

    fn host(&mut self, name: &str) -> Result<bool> {
        host(self, name)
    }

    fn reg(&self, number: u32) -> Result<u64> {
        match number {
            31 => Ok(0),
            30 => self.register(RegisterARM64::LR),
            29 => self.register(RegisterARM64::X29),
            _ => self.arg(number),
        }
    }

    fn set_reg(&mut self, number: u32, value: u64) -> Result<()> {
        match number {
            31 => Ok(()),
            30 => self.write_register(RegisterARM64::LR, value),
            29 => self.write_register(RegisterARM64::X29, value),
            _ => self.set(number, value),
        }
    }

    fn atomic(&mut self, word: u32) -> Result<()> {
        if word & 0x3fa0_7c00 == 0x08a0_7c00 || word & 0xbfa0_7c00 == 0x0820_7c00 {
            return self.compare_exchange(word);
        }
        let (n, t, s) = ((word >> 5) & 31, word & 31, (word >> 16) & 31);
        let address = if n == 31 {
            self.register(RegisterARM64::SP)?
        } else {
            self.reg(n)?
        };
        let size = 1u64 << ((word >> 30) & 3);
        let bits = 64 - size * 8;
        let mut bytes = [0u8; 8];
        bytes[..to_usize(size)].copy_from_slice(&self.read(address, size)?);
        let old = u64::from_le_bytes(bytes);
        let mask = u64::MAX >> bits;
        let rhs = self.reg(s)? & mask;
        let signed = |value: u64| (value << bits).cast_signed() >> bits;
        let value = match (word >> 12) & 15 {
            0 => old.wrapping_add(rhs),
            1 => old & !rhs,
            2 => old ^ rhs,
            3 => old | rhs,
            4 if signed(rhs) > signed(old) => rhs,
            5 if signed(rhs) < signed(old) => rhs,
            4 | 5 => old,
            6 => old.max(rhs),
            7 => old.min(rhs),
            8 => rhs,
            _ => return Err(Error::panic("unsupported atomic")),
        };
        self.write(address, &(value & mask).to_le_bytes()[..to_usize(size)])?;
        self.set_reg(t, old)
    }

    fn compare_exchange(&mut self, word: u32) -> Result<()> {
        let (n, t, s) = ((word >> 5) & 31, word & 31, (word >> 16) & 31);
        let address = if n == 31 {
            self.register(RegisterARM64::SP)?
        } else {
            self.reg(n)?
        };
        let (size, count) = if word & 0xbfa0_7c00 == 0x0820_7c00 {
            (4u64 << ((word >> 30) & 1), 2)
        } else {
            (1u64 << ((word >> 30) & 3), 1)
        };
        let mask = u64::MAX >> (64 - size * 8);
        let mut old = Vec::with_capacity(count as usize);
        let mut matched = true;
        for index in 0..count {
            let mut bytes = [0u8; 8];
            bytes[..to_usize(size)]
                .copy_from_slice(&self.read(address + u64::from(index) * size, size)?);
            let value = u64::from_le_bytes(bytes);
            matched = matched && value == self.reg(s + index)? & mask;
            old.push(value);
        }
        if matched {
            for index in 0..count {
                let value = self.reg(t + index)? & mask;
                self.write(
                    address + u64::from(index) * size,
                    &value.to_le_bytes()[..to_usize(size)],
                )?;
            }
        }
        for (index, value) in (0..).zip(old) {
            self.set_reg(s + index, value)?;
        }
        Ok(())
    }

    /// Go's `observeMetadataCall`: records the caller's return address and stack pointer,
    /// which together match results to the right nested guest invocation.
    fn observe_metadata_call(&mut self, name: &str) -> Result<()> {
        let (name, key, alias): (&'static str, String, String) = match name {
            "swift_getSingletonMetadata" => (
                "swift_getSingletonMetadata",
                self.arg(1)?.to_string(),
                String::new(),
            ),
            "swift_getForeignTypeMetadata" => (
                "swift_getForeignTypeMetadata",
                self.arg(1)?.to_string(),
                String::new(),
            ),
            "swift_getWitnessTable" => {
                let (descriptor, metadata) = (self.arg(0)?, self.arg(1)?);
                (
                    "swift_getWitnessTable",
                    witness_key(descriptor, metadata),
                    descriptor.to_string(),
                )
            }
            _ => return Ok(()),
        };
        let link = self.register(RegisterARM64::LR)?;
        let stack = self.register(RegisterARM64::SP)?;
        self.get_data_mut()
            .metadata_returns
            .entry(link)
            .or_default()
            .push(MetadataReturn {
                stack,
                name,
                key,
                alias,
            });
        Ok(())
    }

    fn observe_metadata_return(&mut self, pc: u64) -> Result<()> {
        let Some(pending) = self.get_data_mut().metadata_returns.remove(&pc) else {
            return Ok(());
        };
        let stack = self.register(RegisterARM64::SP)?;
        let mut remaining = Vec::new();
        for record in pending {
            if record.stack != stack {
                remaining.push(record);
                continue;
            }
            let (value, state) = (self.arg(0)?, self.arg(1)?);
            let metadata = &mut self.get_data_mut().metadata;
            let target = match record.name {
                "swift_getSingletonMetadata" => &mut metadata.swift_singletons,
                "swift_getForeignTypeMetadata" => &mut metadata.swift_foreign,
                _ => &mut metadata.swift_witnesses,
            };
            if value != 0 && (record.name == "swift_getWitnessTable" || state == 0) {
                target.insert(record.key, value);
                // Native reference profiles use descriptor-only keys. Execution uses the
                // metadata-specific key to distinguish generic instances.
                if !record.alias.is_empty() && target.get(&record.alias).copied().unwrap_or(0) == 0
                {
                    target.insert(record.alias, value);
                }
            }
        }
        if !remaining.is_empty() {
            self.get_data_mut().metadata_returns.insert(pc, remaining);
        }
        Ok(())
    }
}

/// Go's `host`: answers a host service and reports whether to return to the caller.
fn host(vm: &mut Vm<'_, '_>, name: &str) -> Result<bool> {
    if let Some(done) = host_memory(vm, name)? {
        return Ok(done);
    }
    if let Some(done) = host_runtime(vm, name)? {
        return Ok(done);
    }
    Ok(host_system(vm, name)?.unwrap_or(false))
}

/// Allocation and C string services over the guest heap.
fn host_memory(vm: &mut Vm<'_, '_>, name: &str) -> Result<Option<bool>> {
    match name {
        "malloc_type_zone_malloc_with_options"
        | "malloc_zone_malloc_with_options"
        | "malloc_zone_memalign"
        | "malloc_type_zone_memalign"
        | "posix_memalign"
        | "malloc_type_posix_memalign" => aligned_alloc(vm, name)?,
        "malloc_size" => {
            let address = vm.arg(0)?;
            let size = vm
                .get_data()
                .allocations
                .get(&address)
                .copied()
                .unwrap_or_default();
            vm.set(0, size)?;
        }
        "malloc_good_size" => {
            let size = vm.arg(0)?;
            vm.set(0, (size + 15) & !15)?;
        }
        "realloc" | "malloc_type_realloc" | "malloc_zone_realloc" | "malloc_type_zone_realloc" => {
            let (pointer, size) = if name.contains("_zone_") {
                (vm.arg(1)?, vm.arg(2)?)
            } else {
                (vm.arg(0)?, vm.arg(1)?)
            };
            if size == 0 {
                vm.set(0, 0)?;
                return Ok(Some(true));
            }
            let address = vm.alloc(size)?;
            if pointer != 0 {
                let old = vm
                    .get_data()
                    .allocations
                    .get(&pointer)
                    .copied()
                    .unwrap_or_default();
                let data = vm.read(pointer, old.min(size))?;
                vm.write(address, &data)?;
            }
            vm.set(0, address)?;
        }
        "_ZnwmSt19__type_descriptor_t"
        | "_ZnamSt19__type_descriptor_t"
        | "_Znwm"
        | "_Znam"
        | "malloc"
        | "calloc"
        | "malloc_zone_malloc"
        | "malloc_zone_calloc"
        | "malloc_type_malloc"
        | "malloc_type_calloc"
        | "malloc_type_zone_malloc"
        | "malloc_type_zone_calloc" => allocate(vm, name)?,
        "memcpy" | "memmove" => {
            let (destination, source, size) = (vm.arg(0)?, vm.arg(1)?, vm.arg(2)?);
            let data = vm.read(source, size)?;
            vm.write(destination, &data)?;
            vm.set(0, destination)?;
        }
        "memset" => {
            let (destination, size) = (vm.arg(0)?, vm.arg(2)?);
            if size > BOOT_HEAP_SIZE {
                return Err(Error::panic("bootstrap memset too large"));
            }
            let value = vm.arg(1)?.to_le_bytes()[0];
            vm.write(destination, &vec![value; to_usize(size)])?;
            vm.set(0, destination)?;
        }
        "bzero" => {
            let size = vm.arg(1)?;
            if size > BOOT_HEAP_SIZE {
                return Err(Error::panic("bootstrap bzero too large"));
            }
            let destination = vm.arg(0)?;
            vm.write(destination, &vec![0; to_usize(size)])?;
        }
        "memcmp" => {
            let size = vm.arg(2)?;
            let (left, right) = (vm.arg(0)?, vm.arg(1)?);
            let left = vm.read(left, size)?;
            let right = vm.read(right, size)?;
            vm.set(0, ordering(left.cmp(&right)))?;
        }
        "strlen" => {
            let text = vm.arg(0)?;
            let length = vm.string(text)?.len() as u64;
            vm.set(0, length)?;
        }
        "strcmp" => {
            let (left, right) = (vm.arg(0)?, vm.arg(1)?);
            let left = vm.string(left)?;
            let right = vm.string(right)?;
            vm.set(0, ordering(left.cmp(&right)))?;
        }
        _ => return Ok(None),
    }
    Ok(Some(true))
}

/// Objective-C runtime and dyld queries answered from the shared-cache metadata.
fn host_runtime(vm: &mut Vm<'_, '_>, name: &str) -> Result<Option<bool>> {
    match name {
        "dyld_program_sdk_at_least" | "dyld_program_minos_at_least" => {
            let requested = vm.arg(0)?;
            let (platform, version) = (requested & 0xffff_ffff, requested >> 32);
            let ok = platform == 1 && version <= 0x001b_0001
                || platform == 0xffff_ffff && version >> 16 <= 2026;
            vm.set(0, u64::from(ok))?;
        }
        "objc_msgSend" => {
            let receiver = vm.arg(0)?;
            let selector_address = vm.arg(1)?;
            let selector = vm.string(selector_address)?;
            if receiver == 0 {
                vm.set(0, 0)?;
                return Ok(Some(true));
            }
            if selector != b"class" {
                return Err(Error::panic(format!(
                    "unhandled bootstrap ObjC message {} to {receiver:#x}",
                    String::from_utf8_lossy(&selector)
                )));
            }
            let class = object_class(vm, receiver)?;
            vm.set(0, class)?;
        }
        "objc_opt_class" => {
            let receiver = vm.arg(0)?;
            if receiver != 0 {
                let class = object_class(vm, receiver)?;
                vm.set(0, class)?;
            }
        }
        "dlopen" => {
            let path = vm.arg(0)?;
            let image = String::from_utf8_lossy(&vm.string(path)?).into_owned();
            let base = vm
                .get_data()
                .source
                .images
                .get(&image)
                .map_or(0, ipatool_sap::assets::StoreImage::base_address);
            vm.set(0, base)?;
        }
        "dlsym" => {
            let symbol = vm.arg(1)?;
            let symbol = String::from_utf8_lossy(&vm.string(symbol)?).into_owned();
            let address = vm
                .get_data()
                .source
                .exports
                .get(&format!("_{symbol}"))
                .copied()
                .unwrap_or_default();
            vm.set(0, address)?;
        }
        "objc_getClass" | "objc_lookUpClass" => {
            let text = vm.arg(0)?;
            let wanted = class_name(&String::from_utf8_lossy(&vm.string(text)?));
            let address = vm
                .get_data()
                .source
                .classes
                .values()
                .filter(|class| class.name == wanted)
                .map(|class| class.address)
                .min()
                .unwrap_or_default();
            vm.set(0, address)?;
        }
        "class_getName" => {
            let class = vm.arg(0)?;
            let name = vm
                .get_data()
                .source
                .classes
                .get(&class)
                .map_or_else(|| "nil".to_owned(), |class| class.name.clone());
            let text = vm.cstring(&name)?;
            vm.set(0, text)?;
        }
        "class_getSuperclass" => {
            let class = vm.arg(0)?;
            let superclass = vm.u64(class + 8)?;
            vm.set(0, superclass)?;
        }
        "class_getInstanceSize" => {
            let class = vm.arg(0)?;
            let size = vm
                .get_data()
                .source
                .classes
                .get(&class)
                .map(|class| class.size)
                .ok_or_else(|| Error::panic("unknown ObjC class size"))?;
            vm.set(0, size)?;
        }
        "object_getClass" => {
            let object = vm.arg(0)?;
            if object != 0 {
                let class = vm.u64(object)?;
                vm.set(0, class)?;
            }
        }
        "_dyld_get_shared_cache_range" => {
            let (base, size) = vm.get_data().source.cache.address_range();
            let output = vm.arg(0)?;
            if output != 0 {
                vm.put(output, size)?;
            }
            vm.set(0, base)?;
        }
        "objc_addLoadImageFunc2"
        | "objc_addLoadImageFunc"
        | "_dyld_register_func_for_add_image" => {
            let function = vm.arg(0)?;
            vm.next_image(function, 0)?;
            return Ok(Some(false));
        }
        "_dyld_lookup_section_info" => lookup_section_info(vm)?,
        "_NSGetMachExecuteHeader" => {
            if vm.get_data().main_header == 0 {
                let header = vm.alloc(32)?;
                vm.get_data_mut().main_header = header;
                let mut data = [0u8; 32];
                data[..4].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
                data[4..8].copy_from_slice(&0x0100_000c_u32.to_le_bytes());
                data[12..16].copy_from_slice(&2u32.to_le_bytes());
                vm.write(header, &data)?;
            }
            let header = vm.get_data().main_header;
            vm.set(0, header)?;
        }
        _ => return Ok(None),
    }
    Ok(Some(true))
}

/// Logging, process, thread, lock and one-time initialization services.
fn host_system(vm: &mut Vm<'_, '_>, name: &str) -> Result<Option<bool>> {
    match name {
        "os_log_create" => {
            let log = vm.alloc(16)?;
            vm.set(0, log)?;
        }
        "os_log_type_enabled"
        | "os_signpost_enabled"
        | "getuid"
        | "geteuid"
        | "getgid"
        | "getegid"
        | "getppid"
        | "_os_feature_enabled_simple_impl"
        | "dlerror"
        | "os_unfair_lock_lock_with_options"
        | "_dyld_is_memory_immutable"
        | "_dyld_has_preoptimized_swift_protocol_conformances"
        | "_dyld_get_swift_prespecialized_data"
        | "dyld_shared_cache_some_image_overridden"
        | "_dyld_swift_optimizations_version"
        | "free"
        | "malloc_zone_free"
        | "pthread_mutex_lock"
        | "pthread_mutex_unlock"
        | "pthread_mutex_init"
        | "pthread_rwlock_rdlock"
        | "pthread_rwlock_wrlock"
        | "pthread_rwlock_unlock"
        | "os_unfair_lock_lock"
        | "os_unfair_lock_unlock"
        | "getenv" => vm.set(0, 0)?,
        "_os_log_impl"
        | "_os_log_error_impl"
        | "_os_log_fault_impl"
        | "objc_readClassPair"
        | "_objc_realizeClassFromSwift"
        | "objc_registerClassPair"
        | "objc_opt_self"
        | "objc_retain"
        | "objc_autorelease"
        | "objc_release"
        | "thread_chkstk_darwin"
        | "___chkstk_darwin"
        | "__chkstk_darwin" => {}
        "csops" => {
            if vm.arg(1)? != 0 || vm.arg(3)? != 4 {
                return Err(Error::panic("unsupported bootstrap code-signing query"));
            }
            let output = vm.arg(2)?;
            vm.write(output, &[0; 4])?;
            vm.set(0, 0)?;
        }
        "getpid" | "pthread_mach_thread_np" => vm.set(0, 1)?,
        "__cxa_guard_acquire" => {
            let guard_address = vm.arg(0)?;
            let guard = vm.u64(guard_address)?;
            if guard & 255 != 0 {
                vm.set(0, 0)?;
            } else if guard & 0xff00 != 0 {
                return Err(Error::panic("recursive C++ static initialization"));
            } else {
                vm.put(guard_address, 0x100)?;
                vm.set(0, 1)?;
            }
        }
        "__cxa_guard_release" => {
            let guard = vm.arg(0)?;
            vm.put(guard, 1)?;
        }
        "__cxa_guard_abort" => {
            let guard = vm.arg(0)?;
            vm.put(guard, 0)?;
        }
        "pthread_key_create" => {
            let state = vm.get_data_mut();
            let key = state.tls.len() as u64 + 1;
            state.tls.insert(key, 0);
            let output = vm.arg(0)?;
            vm.put(output, key)?;
            vm.set(0, 0)?;
        }
        "pthread_key_delete" => {
            let key = vm.arg(0)?;
            vm.get_data_mut().tls.remove(&key);
            vm.set(0, 0)?;
        }
        "pthread_getspecific" => {
            let key = vm.arg(0)?;
            let value = vm.get_data().tls.get(&key).copied().unwrap_or_default();
            vm.set(0, value)?;
        }
        "pthread_setspecific" => {
            let (key, value) = (vm.arg(0)?, vm.arg(1)?);
            vm.get_data_mut().tls.insert(key, value);
            vm.set(0, 0)?;
        }
        "swift_once" | "dispatch_once_f" => {
            let (token, mut function, mut argument) = (vm.arg(0)?, vm.arg(1)?, vm.arg(2)?);
            if name == "dispatch_once_f" {
                (function, argument) = (argument, function);
            }
            match vm.u64(token)? {
                u64::MAX => return Ok(Some(true)),
                0 => {}
                _ => return Err(Error::panic("recursive bootstrap once")),
            }
            vm.put(token, 1)?;
            vm.invoke(function, &[argument], After::Once { token })?;
            return Ok(Some(false));
        }
        "getpagesize" => vm.set(0, 16384)?,
        "pthread_self" => vm.set(0, BOOT_STACK)?,
        "abort" | "__assert_rtn" | "__stack_chk_fail" => {
            return Err(Error::panic(format!("guest called {name}")));
        }
        _ => return Ok(None),
    }
    Ok(Some(true))
}

/// Go's `bytes.Compare`/`strings.Compare` result in a register.
fn ordering(order: Ordering) -> u64 {
    match order {
        Ordering::Less => u64::MAX,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// `objc_msgSend(class)` and `objc_opt_class`: a class object is its own class.
fn object_class(vm: &mut Vm<'_, '_>, receiver: u64) -> Result<u64> {
    if vm.get_data().source.classes.contains_key(&receiver) {
        Ok(receiver)
    } else {
        vm.u64(receiver)
    }
}

fn aligned_alloc(vm: &mut Vm<'_, '_>, name: &str) -> Result<()> {
    let (alignment, size) = (vm.arg(1)?, vm.arg(2)?);
    if alignment == 0 || alignment & (alignment - 1) != 0 || alignment > BOOT_HEAP_SIZE {
        return Err(Error::panic("invalid bootstrap alignment"));
    }
    let state = vm.get_data_mut();
    if state.cursor > BOOT_HEAP_SIZE {
        return Err(Error::panic("bootstrap heap exhausted"));
    }
    state.cursor = (state.cursor + alignment - 1) & !(alignment - 1);
    let address = state.alloc(size)?;
    if name.contains("posix") {
        let output = vm.arg(0)?;
        vm.put(output, address)?;
        vm.set(0, 0)
    } else {
        vm.set(0, address)
    }
}

fn allocate(vm: &mut Vm<'_, '_>, name: &str) -> Result<()> {
    let zone = name.contains("_zone_");
    let mut size = if zone { vm.arg(1)? } else { vm.arg(0)? };
    if name.ends_with("calloc") {
        let count = if zone { vm.arg(2)? } else { vm.arg(1)? };
        if count != 0 && size > BOOT_HEAP_SIZE / count {
            return Err(Error::panic("bootstrap calloc overflow"));
        }
        size = size.wrapping_mul(count);
    }
    let address = vm.alloc(size)?;
    vm.set(0, address)
}

fn lookup_section_info(vm: &mut Vm<'_, '_>) -> Result<()> {
    const SECTIONS: [&str; 7] = [
        "__swift5_protos",
        "__swift5_proto",
        "__swift5_types",
        "__swift5_replace",
        "__swift5_replac2",
        "__swift5_acfuncs",
        "__swift5_ac_pro",
    ];
    let (header, kind) = (vm.arg(0)?, vm.arg(2)?);
    vm.set(0, 0)?;
    vm.set(1, 0)?;
    let wanted = usize::try_from(kind)
        .ok()
        .and_then(|kind| SECTIONS.get(kind))
        .ok_or_else(|| Error::panic("unknown dyld section kind"))?;
    let found = vm
        .get_data()
        .source
        .images
        .values()
        .find(|image| image.base_address() == header)
        .and_then(|image| {
            image
                .sections
                .iter()
                .find(|section| section.name == *wanted)
        })
        .map(|section| (section.address, section.size));
    if let Some((address, size)) = found {
        vm.set(0, address)?;
        vm.set(1, size)?;
    }
    Ok(())
}
