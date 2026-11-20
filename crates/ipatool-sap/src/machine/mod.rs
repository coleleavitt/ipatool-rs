//! The arm64 guest that runs Apple's `CoreFP`, `AppleMediaServices`, `commerce`, `adid`,
//! `fairplayd` and the `FairPlay` kernel extension. Port of Go `internal/sap/machine`.

mod arm64;
mod ioaes;
mod kbsync;
mod macos27;
mod modern_adi;
#[cfg(test)]
mod oracle_tests;
mod shim_memory;
mod shim_platform;
mod shims;
mod storeagent;

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::time::Duration;

pub use kbsync::generate_kb_sync;
pub use modern_adi::ModernAdi;
pub use storeagent::StoreAgent;

use shims::{Guest, ShimOptions, State, Uc};

use crate::assets::Bundle;
use crate::engine;
use crate::error::{Context, Error, Result};
use crate::machimage::{Image, Memory};

const SAP_GUEST_TIMEOUT: Duration = Duration::from_secs(60);

const RETURN_ADDRESS: u64 = 0x0000_0001_0000_0000;
const CORE_FP_BASE: u64 = 0x0000_1000_0000_0000;
const SCRATCH_BASE: u64 = 0x0000_3000_0000_0000;
const SCRATCH_SIZE: u64 = 32 << 20;
pub(crate) const HEAP_BASE: u64 = 0x0000_4000_0000_0000;
pub(crate) const HEAP_SIZE: u64 = 64 << 20;
const STACK_BASE: u64 = 0x0000_5000_0000_0000;
const STACK_SIZE: u64 = 8 << 20;
const STACK_END: u64 = STACK_BASE + STACK_SIZE;
pub(crate) const PAGE_SIZE: u64 = 0x1000;
const MAX_OUTPUT_SIZE: u64 = 16 << 20;

const CORE_EXPORT_NAMES: [&str; 6] = [
    "_WIn9UJ86JKdV4dM",
    "_X46O5IeS",
    "_YlCJ3lg",
    "_dku592fbFAj",
    "_fdjkDSAFjklaf2s",
    "_lxpgvVMLd0S7uRl",
];

const SAP_EXPORT_NAMES: [&str; 5] = [
    "_cp2g1b9ro",
    "_Mib5yocT",
    "_Fc3vhtJDvr",
    "_IPaI1oem5iL",
    "_jEHf8Xzsv8K",
];

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EntryPoints {
    initialize: u64,
    exchange: u64,
    sign: u64,
    teardown: u64,
    pub(crate) dispose: u64,
}

/// One emulator with its guest state. Clones share the same engine.
#[derive(Clone)]
pub struct Machine {
    uc: Uc<'static>,
}

struct ImageSpec<'a> {
    name: &'static str,
    data: &'a [u8],
    base: u64,
    prelinked: Option<&'static [(u64, &'static str)]>,
    slots: &'static [(u64, &'static str)],
    functions: &'static [(u64, &'static str)],
    kernel_base: u64,
}

impl<'a> ImageSpec<'a> {
    fn new(name: &'static str, data: &'a [u8], base: u64) -> Self {
        Self {
            name,
            data,
            base,
            prelinked: None,
            slots: &[],
            functions: &[],
            kernel_base: 0,
        }
    }
}

#[derive(Default)]
struct RuntimeOptions<'a> {
    extra_images: Vec<ImageSpec<'a>>,
    shims: ShimOptions,
}

impl Memory for Uc<'_> {
    fn mem_map(&mut self, address: u64, size: u64) -> Result<()> {
        self.map_mem(address, size)
    }

    fn mem_write(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.write_mem(address, data)
    }
}

impl Machine {
    /// Go's `machine.Open`: the `CoreFP` + `AppleMediaServices` SAP runtime.
    pub fn open(bundle: &Bundle) -> Result<Self> {
        let (mut machine, exports) = open_runtime(bundle, RuntimeOptions::default())?;
        machine.state_mut().entry = EntryPoints {
            initialize: exports["_cp2g1b9ro"],
            exchange: exports["_Mib5yocT"],
            sign: exports["_Fc3vhtJDvr"],
            teardown: exports["_IPaI1oem5iL"],
            dispose: exports["_jEHf8Xzsv8K"],
        };
        Ok(machine)
    }

    fn state(&self) -> &State {
        self.uc.get_data()
    }

    fn state_mut(&mut self) -> &mut State {
        self.uc.get_data_mut()
    }

    pub fn initialize(&mut self, hardware_id: &[u8]) -> Result<u64> {
        let hardware = hardware_block(hardware_id)?;
        self.begin_call();
        let result = self.initialize_call(&hardware);
        self.clear_scratch();
        result
    }

    fn initialize_call(&mut self, hardware: &[u8]) -> Result<u64> {
        let context_field = self.scratch(&[], 8)?;
        let hardware_address = self.scratch(hardware, hardware.len() as u64)?;
        let entry = self.state().entry.initialize;
        let status = self.invoke(entry, &[context_field, hardware_address])?;
        let status = status_code(status);
        if status != 0 {
            return Err(Error::msg(format!("SAP initialization returned {status}")));
        }
        let context = self.read_u64(context_field)?;
        if context == 0 {
            return Err(Error::msg("SAP initialization returned a null context"));
        }
        Ok(context)
    }

    /// Runs one SAP setup step, returning the next message and the session state.
    pub fn exchange(
        &mut self,
        version: u32,
        hardware_id: &[u8],
        context: u64,
        input: &[u8],
    ) -> Result<(Vec<u8>, i32)> {
        if u32::try_from(input.len()).is_err() {
            return Err(Error::msg("SAP exchange input is too large"));
        }
        let hardware = hardware_block(hardware_id)?;
        self.begin_call();
        let result = self.exchange_call(version, &hardware, context, input);
        self.clear_scratch();
        result
    }

    fn exchange_call(
        &mut self,
        version: u32,
        hardware: &[u8],
        context: u64,
        input: &[u8],
    ) -> Result<(Vec<u8>, i32)> {
        let hardware_address = self.scratch(hardware, hardware.len() as u64)?;
        let input_address = self.scratch(input, input.len() as u64)?;
        let output_field = self.scratch(&[], 8)?;
        let length_field = self.scratch(&[], 8)?;
        let result_field = self.scratch(&[], 4)?;
        let entry = self.state().entry.exchange;
        let status = self.invoke(
            entry,
            &[
                u64::from(version),
                hardware_address,
                context,
                input_address,
                input.len() as u64,
                output_field,
                length_field,
                result_field,
            ],
        )?;
        let status = status_code(status);
        if status != 0 {
            return Err(Error::msg(format!("SAP exchange returned {status}")));
        }
        let output = self.consume_output(output_field, length_field)?;
        let result = self.read_u32(result_field)?;
        Ok((output, result.cast_signed()))
    }

    pub fn sign(&mut self, context: u64, input: &[u8]) -> Result<Vec<u8>> {
        if u32::try_from(input.len()).is_err() {
            return Err(Error::msg("SAP signing input is too large"));
        }
        self.begin_call();
        let result = self.sign_call(context, input);
        self.clear_scratch();
        result
    }

    fn sign_call(&mut self, context: u64, input: &[u8]) -> Result<Vec<u8>> {
        let input_address = self.scratch(input, input.len() as u64)?;
        let output_field = self.scratch(&[], 8)?;
        let length_field = self.scratch(&[], 8)?;
        let entry = self.state().entry.sign;
        let status = self.invoke(
            entry,
            &[
                context,
                input_address,
                input.len() as u64,
                output_field,
                length_field,
            ],
        )?;
        let status = status_code(status);
        if status != 0 {
            return Err(Error::msg(format!("SAP signing returned {status}")));
        }
        self.consume_output(output_field, length_field)
    }

    pub fn teardown(&mut self, context: u64) -> Result<()> {
        let entry = self.state().entry.teardown;
        let status = status_code(self.invoke(entry, &[context])?);
        if status != 0 {
            return Err(Error::msg(format!("SAP teardown returned {status}")));
        }
        Ok(())
    }

    fn dispose(&mut self, output: u64) -> Result<()> {
        let entry = self.state().entry.dispose;
        let status = status_code(self.invoke(entry, &[output])?);
        if status != 0 {
            return Err(Error::msg(format!(
                "SAP storage disposal returned {status}"
            )));
        }
        Ok(())
    }

    /// Calls a guest function with the arm64 calling convention and runs it until it returns
    /// to [`RETURN_ADDRESS`].
    fn invoke(&mut self, function: u64, arguments: &[u64]) -> Result<u64> {
        if self.state().closed {
            return Err(Error::msg("SAP guest machine is closed"));
        }
        if function == 0 {
            return Err(Error::msg("SAP guest entry point is unavailable"));
        }
        for (index, register) in shims::ARGUMENT_REGISTERS.into_iter().enumerate() {
            let value = arguments.get(index).copied().unwrap_or_default();
            self.uc
                .set_reg(register, value)
                .context("write SAP guest argument register")?;
        }
        let extra = arguments
            .get(shims::ARGUMENT_REGISTERS.len()..)
            .unwrap_or_default();
        let mut stack_pointer = STACK_END - extra.len() as u64 * 8;
        stack_pointer -= stack_pointer % 16;
        for (index, &value) in extra.iter().enumerate() {
            self.write_u64(stack_pointer + index as u64 * 8, value)?;
        }
        self.uc
            .set_reg(shims::REG_SP, stack_pointer)
            .context("write SAP guest stack register")?;
        self.uc
            .set_reg(shims::REG_LR, RETURN_ADDRESS)
            .context("write SAP guest return register")?;
        self.state_mut().fault = None;

        // SAP's cryptographic routines have input- and host-dependent instruction counts.
        // Bound execution by wall time, not by a fixed instruction limit.
        let mut next = function;
        while let Err(execution) =
            engine::start_bounded(&mut self.uc, next, RETURN_ADDRESS, SAP_GUEST_TIMEOUT, 0)
        {
            if let Some(fault) = self.fault() {
                return Err(fault);
            }
            if self.state().arm64 && arm64::emulate_invalid_instruction(&mut self.uc)? {
                next = self
                    .uc
                    .reg(shims::REG_PC)
                    .context("resume SAP guest function")?;
                continue;
            }
            let Ok(instruction) = self.uc.reg(shims::REG_PC) else {
                return Err(execution.wrap("execute SAP guest function"));
            };
            let Ok(link) = self.uc.reg(shims::REG_LR) else {
                return Err(
                    execution.wrap(format!("execute SAP guest function at {instruction:#x}"))
                );
            };
            return Err(execution.wrap(format!(
                "execute SAP guest function at {instruction:#x} from {link:#x}"
            )));
        }
        if let Some(fault) = self.fault() {
            return Err(fault);
        }
        let instruction = self
            .uc
            .reg(shims::REG_PC)
            .context("read SAP guest instruction register")?;
        if instruction != RETURN_ADDRESS {
            return Err(Error::msg(format!(
                "SAP guest stopped unexpectedly at {instruction:#x}"
            )));
        }
        self.uc
            .reg(shims::REG_X0)
            .context("read SAP guest result register")
    }

    /// The recorded service failure. It stays recorded, as in Go, so a service that wraps the
    /// error of a nested call on this machine does not replace the first failure.
    fn fault(&self) -> Option<Error> {
        self.state()
            .fault
            .as_ref()
            .map(|fault| Error::msg(fault.to_string()))
    }

    fn begin_call(&mut self) {
        self.state_mut().scratch_cursor = 0;
    }

    fn scratch(&mut self, data: &[u8], size: u64) -> Result<u64> {
        let reserved = align(size.max(1), 16);
        let cursor = self.state().scratch_cursor;
        if cursor > SCRATCH_SIZE || reserved > SCRATCH_SIZE - cursor {
            return Err(Error::msg("SAP guest scratch space exhausted"));
        }
        let address = SCRATCH_BASE + cursor;
        self.state_mut().scratch_cursor += reserved;
        if !data.is_empty() {
            if data.len() as u64 > size {
                return Err(Error::msg("scratch data exceeds reservation"));
            }
            self.uc
                .write_mem(address, data)
                .context("write SAP guest scratch data")?;
        } else if size != 0 {
            self.uc
                .write_mem(address, &shim_memory::zeroes(size)?)
                .context("clear SAP guest scratch data")?;
        }
        Ok(address)
    }

    fn clear_scratch(&mut self) {
        let cursor = self.state().scratch_cursor;
        if cursor != 0
            && !self.state().closed
            && let Ok(zeroes) = shim_memory::zeroes(cursor)
        {
            // Go ignores this error too: the call's own result is already decided.
            let _ = self.uc.write_mem(SCRATCH_BASE, &zeroes);
        }
        self.state_mut().scratch_cursor = 0;
    }

    fn consume_output(&mut self, pointer_field: u64, length_field: u64) -> Result<Vec<u8>> {
        let pointer = self.read_u64(pointer_field)?;
        let length = self.read_u64(length_field)?;
        let (output, output_error) = if length > MAX_OUTPUT_SIZE {
            (
                Vec::new(),
                Some(Error::msg(format!(
                    "SAP output is {length} bytes, maximum is {MAX_OUTPUT_SIZE}"
                ))),
            )
        } else if length == 0 {
            (Vec::new(), None)
        } else if pointer == 0 {
            (
                Vec::new(),
                Some(Error::msg("SAP returned a null output pointer")),
            )
        } else {
            match self.uc.read_mem(pointer, length) {
                Ok(output) => (output, None),
                Err(error) => (Vec::new(), Some(error)),
            }
        };
        let dispose_error = if pointer == 0 {
            None
        } else {
            self.dispose(pointer).err()
        };
        match Error::join([output_error, dispose_error]) {
            Some(error) => Err(error),
            None => Ok(output),
        }
    }

    fn read_u32(&self, address: u64) -> Result<u32> {
        let data = self
            .uc
            .read_mem(address, 4)
            .context("read SAP guest uint32")?;
        Ok(shims::le32(&data))
    }

    fn read_u64(&self, address: u64) -> Result<u64> {
        let data = self
            .uc
            .read_mem(address, 8)
            .context("read SAP guest uint64")?;
        Ok(shims::le64(&data))
    }

    fn read_mem_into(&self, data: &mut [u8], address: u64) -> Result<()> {
        self.uc.read_mem_into(data, address)
    }

    fn write_u64(&mut self, address: u64, value: u64) -> Result<()> {
        self.uc
            .write_mem(address, &value.to_le_bytes())
            .context("write SAP guest uint64")
    }

    /// Go's `Machine.Close`: later calls fail, and host callbacks are dropped so machines that
    /// reference each other are freed.
    pub fn close(&mut self) -> Result<()> {
        if self.state().closed {
            return Ok(());
        }
        self.state_mut().closed = true;
        let result = shims::close(&mut self.uc);
        self.state_mut().release_handlers();
        result
    }
}

fn open_runtime(
    bundle: &Bundle,
    options: RuntimeOptions<'_>,
) -> Result<(Machine, HashMap<String, u64>)> {
    let mut specs = vec![
        ImageSpec::new("CoreFP", &bundle.core_fp, CORE_FP_BASE),
        ImageSpec {
            prelinked: Some(macos27::SHARED_CACHE_FUNCTIONS),
            slots: macos27::SHARED_CACHE_SLOTS,
            functions: macos27::SHARED_CACHE_FUNCTIONS,
            ..ImageSpec::new("AppleMediaServices", &bundle.apple_media_services, 0)
        },
    ];
    specs.extend(options.extra_images);

    let mut images = Vec::with_capacity(specs.len());
    for spec in &specs {
        let image = Image::open_shared(spec.name, spec.data)
            .with_context(|| format!("open {} image", spec.name))?;
        images.push(image);
    }

    let mut exports = HashMap::new();
    let mut core_exports = HashMap::new();
    for name in CORE_EXPORT_NAMES {
        let address = images[0]
            .export(name, CORE_FP_BASE)
            .with_context(|| format!("resolve CoreFP export {name}"))?;
        exports.insert(name.to_owned(), address);
        core_exports.insert(name.to_owned(), address);
    }
    let apple_media_services = &images[1];
    for name in SAP_EXPORT_NAMES {
        let address = apple_media_services
            .export(name, apple_media_services.base())
            .with_context(|| format!("resolve AppleMediaServices export {name}"))?;
        exports.insert(name.to_owned(), address);
    }

    let state = State::new(
        core_exports,
        Rc::from(bundle.core_fp_icxs.as_slice()),
        options.shims,
    );
    let mut uc = engine::new(state).context("create Unicorn engine")?;
    uc.get_data_mut().arm64 = true;
    for (address, size) in [
        (RETURN_ADDRESS, PAGE_SIZE),
        (SCRATCH_BASE, SCRATCH_SIZE),
        (HEAP_BASE, HEAP_SIZE),
        (STACK_BASE, STACK_SIZE),
    ] {
        uc.map_mem(address, size)
            .with_context(|| format!("map SAP guest memory at {address:#x}"))?;
    }
    uc.write_mem(RETURN_ADDRESS, &[0x00, 0x00, 0x20, 0xd4])
        .context("write SAP guest return instruction")?;
    shims::install(&mut uc, options.shims)?;

    let mut resolver = |uc: &mut Uc, name: &str| match exports.get(name) {
        Some(&address) => Ok(address),
        None => shims::resolve(uc, name),
    };
    for spec in &specs {
        load_prelinked_slots(&mut uc, spec.slots, &mut resolver)
            .with_context(|| format!("load {} shared-cache slots", spec.name))?;
        load_prelinked_functions(&mut uc, spec.functions, &mut resolver)
            .with_context(|| format!("load {} shared-cache functions", spec.name))?;
    }
    for (spec, image) in specs.iter().zip(&mut images) {
        let result = {
            let mut resolve = |name: &str| resolver(&mut uc, name);
            match spec.prelinked {
                Some(prelinked) => image.prepare_prelinked(prelinked, &mut resolve),
                None => image.relocate(spec.base, &mut resolve),
            }
        };
        result.context("relocate SAP guest image")?;
        if spec.kernel_base != 0 {
            let mut resolve = |name: &str| resolver(&mut uc, name);
            image
                .relocate_kernel_cache_pointers(spec.base, spec.kernel_base, &mut resolve)
                .with_context(|| format!("relocate {} kernel-cache pointers", spec.name))?;
        }
        image.load(&mut uc).context("load SAP guest image")?;
    }
    Ok((Machine { uc }, exports))
}

fn load_prelinked_functions(
    uc: &mut Uc,
    functions: &[(u64, &str)],
    resolve: &mut impl FnMut(&mut Uc, &str) -> Result<u64>,
) -> Result<()> {
    let mut pages = BTreeSet::new();
    for &(address, _) in functions {
        pages.insert(address - address % PAGE_SIZE);
        let end = address + 15;
        pages.insert(end - end % PAGE_SIZE);
    }
    for address in pages {
        uc.map_mem(address, PAGE_SIZE)
            .with_context(|| format!("map function page at {address:#x}"))?;
    }
    for &(address, name) in functions {
        let target = resolve(uc, name).with_context(|| format!("resolve {name}"))?;
        let mut trampoline = [0u8; 16];
        trampoline[0..4].copy_from_slice(&0x5800_0050u32.to_le_bytes()); // ldr x16, #8
        trampoline[4..8].copy_from_slice(&0xd61f_0200u32.to_le_bytes()); // br x16
        trampoline[8..16].copy_from_slice(&target.to_le_bytes());
        uc.write_mem(address, &trampoline)
            .with_context(|| format!("write {name} trampoline at {address:#x}"))?;
    }
    Ok(())
}

fn load_prelinked_slots(
    uc: &mut Uc,
    slots: &[(u64, &str)],
    resolve: &mut impl FnMut(&mut Uc, &str) -> Result<u64>,
) -> Result<()> {
    let pages: BTreeSet<u64> = slots
        .iter()
        .map(|&(address, _)| address - address % PAGE_SIZE)
        .collect();
    for address in pages {
        uc.map_mem(address, PAGE_SIZE)
            .with_context(|| format!("map slot page at {address:#x}"))?;
    }
    for &(address, name) in slots {
        let value = resolve(uc, name).with_context(|| format!("resolve {name}"))?;
        uc.write_mem(address, &value.to_le_bytes())
            .with_context(|| format!("write {name} slot at {address:#x}"))?;
    }
    Ok(())
}

/// The 24-byte block SAP takes: a little-endian length followed by the identifier.
fn hardware_block(hardware_id: &[u8]) -> Result<Vec<u8>> {
    if hardware_id.is_empty() || hardware_id.len() > 20 {
        return Err(Error::msg(
            "hardware ID must contain between 1 and 20 bytes",
        ));
    }
    let mut result = vec![0; 24];
    let length = u32::try_from(hardware_id.len()).unwrap_or(u32::MAX);
    result[..4].copy_from_slice(&length.to_le_bytes());
    result[4..4 + hardware_id.len()].copy_from_slice(hardware_id);
    Ok(result)
}

pub(crate) fn align(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

/// Go's `int32(status)` on a guest return register.
fn status_code(value: u64) -> i32 {
    shims::low32(value).cast_signed()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::shims::Abi;
    use super::*;

    /// Go's `newServiceTestMachine`: the guest memory and services without any Apple image.
    pub(crate) fn service_machine(options: ShimOptions) -> Machine {
        let state = State::new(HashMap::new(), Rc::from(Vec::new()), options);
        let mut uc = engine::new(state).unwrap();
        for (address, size) in [
            (RETURN_ADDRESS, PAGE_SIZE),
            (SCRATCH_BASE, SCRATCH_SIZE),
            (HEAP_BASE, HEAP_SIZE),
            (STACK_BASE, STACK_SIZE),
        ] {
            uc.map_mem(address, size).unwrap();
        }
        uc.write_mem(RETURN_ADDRESS, &[0x00, 0x00, 0x20, 0xd4])
            .unwrap();
        shims::install(&mut uc, options).unwrap();
        Machine { uc }
    }

    fn symbol(machine: &Machine, name: &str) -> u64 {
        machine.state().symbols[name]
    }

    #[test]
    fn hardware_block_has_length_header() {
        let identifier = [1, 2, 3, 4, 5, 6];
        let block = hardware_block(&identifier).unwrap();
        assert_eq!(block.len(), 24);
        assert_eq!(shims::le32(&block), 6);
        assert_eq!(block[4..10], identifier);
        assert!(hardware_block(&[0; 21]).is_err());
    }

    #[test]
    fn dispatches_services_with_stack_arguments() {
        let mut machine = service_machine(ShimOptions::default());
        let address = shims::add_function(
            &mut machine.uc,
            "test.stackArguments",
            Rc::new(|uc| {
                let seventh = uc.argument(6)?;
                let eighth = uc.argument(7)?;
                uc.set_result(seventh + eighth)
            }),
        )
        .unwrap();
        assert_eq!(
            machine.invoke(address, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap(),
            15
        );
        let ninth = shims::add_function(
            &mut machine.uc,
            "test.ninthArgument",
            Rc::new(|uc| {
                let ninth = uc.argument(8)?;
                uc.set_result(ninth)
            }),
        )
        .unwrap();
        assert_eq!(
            machine
                .invoke(ninth, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
                .unwrap(),
            9
        );
    }

    #[test]
    fn unknown_guest_service_fails_closed() {
        let mut machine = service_machine(ShimOptions::default());
        let address = shims::resolve(&mut machine.uc, "test.unsupported").unwrap();
        let error = machine.invoke(address, &[]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "test.unsupported: guest called unsupported import test.unsupported"
        );
    }

    #[test]
    fn allocator_reuses_and_clears_freed_memory() {
        let mut machine = service_machine(ShimOptions::default());
        let address = machine.state_mut().allocate(64).unwrap();
        machine.uc.write_mem(address, &[0xa5; 64]).unwrap();
        shim_memory::release(&mut machine.uc, address).unwrap();
        let reused = machine.state_mut().allocate(32).unwrap();
        assert_eq!(reused, address);
        assert_eq!(machine.uc.read_mem(reused, 32).unwrap(), [0; 32]);
        shim_memory::release(&mut machine.uc, reused).unwrap();
        assert_eq!(machine.state().heap_cursor, 0);
        assert_eq!(machine.state().free_blocks, []);
    }

    #[test]
    fn allocator_coalesces_neighbouring_free_blocks() {
        let mut machine = service_machine(ShimOptions::default());
        let first = machine.state_mut().allocate(16).unwrap();
        let second = machine.state_mut().allocate(16).unwrap();
        let third = machine.state_mut().allocate(16).unwrap();
        shim_memory::release(&mut machine.uc, first).unwrap();
        shim_memory::release(&mut machine.uc, second).unwrap();
        assert_eq!(
            machine.state().free_blocks,
            [shims::FreeBlock {
                address: first,
                size: 32
            }]
        );
        assert_eq!(machine.state_mut().allocate(48).unwrap(), third + 16);
        assert_eq!(machine.state_mut().allocate(20).unwrap(), first);
    }

    #[test]
    fn malloc_size_returns_usable_size() {
        let mut machine = service_machine(ShimOptions::default());
        let address = machine.state_mut().allocate(1).unwrap();
        let malloc_size = symbol(&machine, "_malloc_size");
        assert_eq!(machine.invoke(malloc_size, &[address]).unwrap(), 16);
    }

    #[test]
    fn strncmp_stops_at_mapped_page_boundary() {
        let mut machine = service_machine(ShimOptions::default());
        let left = RETURN_ADDRESS + PAGE_SIZE - 2;
        let right = STACK_BASE + PAGE_SIZE - 2;
        for address in [left, right] {
            machine.uc.write_mem(address, b"a\0").unwrap();
        }
        let strncmp = symbol(&machine, "_strncmp");
        assert_eq!(machine.invoke(strncmp, &[left, right, 64]).unwrap(), 0);
    }

    #[test]
    fn platform_services_initialize_outputs() {
        let mut machine = service_machine(ShimOptions::default());
        let buffer = STACK_BASE + PAGE_SIZE;
        machine.uc.write_mem(buffer, &[0xa5; 16]).unwrap();
        let get_c_string = symbol(&machine, "_CFStringGetCString");
        assert_eq!(
            machine
                .invoke(get_c_string, &[shim_platform::FAKE_HANDLE, buffer, 16, 0])
                .unwrap(),
            1
        );
        assert_eq!(machine.uc.read_mem(buffer, 1).unwrap(), [0]);
        for name in [
            "_IORegistryEntryGetParentEntry",
            "_IOServiceGetMatchingServices",
        ] {
            machine.uc.write_mem(buffer, &[0xa5; 4]).unwrap();
            let service = symbol(&machine, name);
            assert_eq!(
                machine.invoke(service, &[0, 0, buffer]).unwrap(),
                0,
                "{name}"
            );
            assert_eq!(machine.uc.read_mem(buffer, 4).unwrap(), [0xff; 4], "{name}");
        }
        let statfs = symbol(&machine, "_statfs");
        assert_eq!(machine.invoke(statfs, &[0, buffer]).unwrap(), u64::MAX);
    }

    #[test]
    fn consume_output_disposes_allocation() {
        let mut machine = service_machine(ShimOptions::default());
        let output = machine.state_mut().allocate(4).unwrap();
        machine.uc.write_mem(output, b"test").unwrap();
        machine.write_u64(SCRATCH_BASE, output).unwrap();
        machine.write_u64(SCRATCH_BASE + 8, 4).unwrap();
        let dispose = shims::add_function(
            &mut machine.uc,
            "test.dispose",
            shims::handler(shim_memory::free),
        )
        .unwrap();
        machine.state_mut().entry.dispose = dispose;
        let data = machine
            .consume_output(SCRATCH_BASE, SCRATCH_BASE + 8)
            .unwrap();
        assert_eq!(data, b"test");
        assert!(!machine.state().allocations.contains_key(&output));
    }

    #[test]
    fn closed_machine_rejects_calls() {
        let mut machine = service_machine(ShimOptions::default());
        machine.close().unwrap();
        machine.close().unwrap();
        let error = machine.invoke(RETURN_ADDRESS, &[]).unwrap_err();
        assert_eq!(error.to_string(), "SAP guest machine is closed");
    }

    #[test]
    fn runs_pthread_initializer_once_with_valid_call_frame() {
        let mut machine = service_machine(ShimOptions::default());
        let control = SCRATCH_BASE;
        machine.write_u64(control, 0x30b1_bcba).unwrap();
        let calls = Rc::new(std::cell::Cell::new(0));
        let counter = calls.clone();
        let initializer = shims::add_function(
            &mut machine.uc,
            "test.pthread.initializer",
            Rc::new(move |uc| {
                if uc.reg(shims::REG_SP)? % 16 != 0 {
                    return Err(Error::msg("misaligned stack"));
                }
                counter.set(counter.get() + 1);
                uc.set_result(0xdead_beef)
            }),
        )
        .unwrap();
        let once = symbol(&machine, "_pthread_once");
        for _ in 0..2 {
            assert_eq!(machine.invoke(once, &[control, initializer]).unwrap(), 0);
        }
        assert_eq!(calls.get(), 1);
        assert_eq!(machine.read_u64(control).unwrap(), 0x4f4e_4345);
    }

    #[test]
    fn runs_dispatch_block_once() {
        let mut machine = service_machine(ShimOptions::default());
        let predicate = SCRATCH_BASE;
        let block = SCRATCH_BASE + 0x100;
        let calls = Rc::new(std::cell::Cell::new(0));
        let counter = calls.clone();
        let initializer = shims::add_function(
            &mut machine.uc,
            "test.dispatch.initializer",
            Rc::new(move |uc| {
                if uc.argument(0)? != block {
                    return Err(Error::msg("dispatch block is not the first argument"));
                }
                counter.set(counter.get() + 1);
                uc.set_result(0xdead_beef)
            }),
        )
        .unwrap();
        machine.write_u64(block + 16, initializer).unwrap();
        let once = symbol(&machine, "_dispatch_once");
        for _ in 0..2 {
            assert_eq!(machine.invoke(once, &[predicate, block]).unwrap(), 0);
        }
        assert_eq!(calls.get(), 1);
        assert_eq!(machine.read_u64(predicate).unwrap(), u64::MAX);
    }

    #[test]
    fn register_numbers_match_unicorn() {
        use unicorn_engine::RegisterARM64;
        assert_eq!(i32::from(RegisterARM64::X0), shims::REG_X0);
        assert_eq!(i32::from(RegisterARM64::X29), shims::REG_X29);
        assert_eq!(i32::from(RegisterARM64::X30), shims::REG_LR);
        assert_eq!(i32::from(RegisterARM64::SP), shims::REG_SP);
        assert_eq!(i32::from(RegisterARM64::V0), shims::REG_V0);
        assert_eq!(i32::from(RegisterARM64::PC), shims::REG_PC);
        assert_eq!(i32::from(RegisterARM64::X28), shims::REG_X0 + 28);
        assert_eq!(i32::from(RegisterARM64::V31), shims::REG_V0 + 31);
    }
}
