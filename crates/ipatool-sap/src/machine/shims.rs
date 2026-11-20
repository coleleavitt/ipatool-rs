//! Guest service table: one `ret` stub per imported symbol, dispatched from a single code hook.

use std::collections::HashMap;
use std::rc::Rc;

use unicorn_engine::{Prot, UcHookId, Unicorn};

use super::{EntryPoints, align};
use crate::engine::UcResult;
use crate::error::{Context, Error, Result};

pub(crate) const SHIM_BASE: u64 = 0x0000_7ff8_8000_0000;
pub(crate) const SHIM_CODE_SIZE: u64 = 0x0000_0000_0008_0000;
pub(crate) const SHIM_SIZE: u64 = 0x0000_0000_0010_0000;
const SHIM_SLOT_SIZE: u64 = 16;
pub(crate) const MAX_GUEST_TRANSFER: u64 = 64 << 20;

pub(crate) const REG_X29: i32 = 1;
pub(crate) const REG_LR: i32 = 2;
pub(crate) const REG_SP: i32 = 4;
pub(crate) const REG_X0: i32 = 199;
pub(crate) const REG_V0: i32 = 228;
pub(crate) const REG_PC: i32 = 260;
pub(crate) const ARGUMENT_REGISTERS: [i32; 8] = [199, 200, 201, 202, 203, 204, 205, 206];

pub(crate) type Uc<'a> = Unicorn<'a, State>;
pub(crate) type Handler = Rc<dyn Fn(&mut Uc) -> Result<()>>;
pub(crate) type Service = fn(&mut Uc<'_>) -> Result<()>;
/// Answers a `FairPlay` `IOKit` call: `(input, indirect, output size) -> (output, indirect output, code)`.
pub(crate) type FairPlayHandler =
    Rc<dyn Fn(&[u8], &[u8], usize) -> Result<(Vec<u8>, Vec<u8>, i32)>>;
pub(crate) type MachRpcHandler = Rc<dyn Fn(&[u8], usize) -> Result<Vec<u8>>>;

#[derive(Clone)]
struct ShimEntry {
    name: Rc<str>,
    handler: Handler,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GuestAllocation {
    pub(crate) size: u64,
    pub(crate) reserved: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FreeBlock {
    pub(crate) address: u64,
    pub(crate) size: u64,
}

#[derive(Default)]
pub(crate) struct XpcObject {
    pub(crate) data: Vec<u8>,
    pub(crate) data_address: u64,
    pub(crate) has_data: bool,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ShimOptions {
    pub(crate) zero_return_aliases: &'static [&'static str],
    pub(crate) pause_run_loop: bool,
}

/// Guest-visible host state: Go's `shims` plus the per-call fields of `Machine`.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools, reason = "Go's flat guest state")]
pub(crate) struct State {
    entries: HashMap<u64, ShimEntry>,
    pub(crate) symbols: HashMap<String, u64>,
    code_cursor: u64,
    data_cursor: u64,
    hook: Option<UcHookId>,
    pub(crate) fault: Option<Error>,
    pub(crate) core_exports: HashMap<String, u64>,
    pub(crate) icxs: Rc<[u8]>,
    pub(crate) icxs_offset: usize,
    pub(crate) errno: u64,
    pub(crate) heap_cursor: u64,
    pub(crate) allocations: HashMap<u64, GuestAllocation>,
    pub(crate) free_blocks: Vec<FreeBlock>,
    pub(crate) iterator: u32,
    pub(crate) callback_returns: Vec<u64>,
    pub(crate) xpc_data: Vec<u8>,
    pub(crate) xpc_objects: HashMap<u64, XpcObject>,
    pub(crate) xpc_next_object: u64,
    pub(crate) xpc_event_handler: u64,
    pub(crate) xpc_reply: Vec<u8>,
    pub(crate) xpc_replied: bool,
    pub(crate) xpc_sync_reply: Option<Vec<u8>>,
    pub(crate) mach_port_callback: u64,
    pub(crate) mach_port_context: u64,
    pub(crate) mach_port_info: u64,
    pub(crate) fair_play_handler: Option<FairPlayHandler>,
    pub(crate) mach_rpc_handler: Option<MachRpcHandler>,
    pub(crate) mach_message_reply: Vec<u8>,
    pub(crate) adi_files: HashMap<String, Vec<u8>>,
    pub(crate) adi_path: String,
    pub(crate) adi_offset: usize,
    pub(crate) host_uuid: [u8; 16],
    pub(crate) pause_run_loop: bool,
    pub(crate) aes_vtable: u64,
    pub(crate) aes_service: u64,
    pub(crate) buffer_vtable: u64,

    pub(crate) entry: EntryPoints,
    pub(crate) scratch_cursor: u64,
    pub(crate) arm64: bool,
    pub(crate) closed: bool,
    pub(crate) host: super::shim_platform::Host,
}

impl State {
    pub(crate) fn new(
        core_exports: HashMap<String, u64>,
        icxs: Rc<[u8]>,
        options: ShimOptions,
    ) -> Self {
        Self {
            code_cursor: SHIM_BASE,
            data_cursor: SHIM_BASE + SHIM_CODE_SIZE,
            core_exports,
            icxs,
            xpc_next_object: 0xffff_0000_0000_0000,
            pause_run_loop: options.pause_run_loop,
            ..Self::default()
        }
    }

    pub(crate) fn new_xpc_object(&mut self, data: Option<&[u8]>) -> u64 {
        self.xpc_next_object += 1;
        self.xpc_objects.insert(
            self.xpc_next_object,
            XpcObject {
                data: data.map_or_else(Vec::new, <[u8]>::to_vec),
                data_address: 0,
                has_data: data.is_some(),
            },
        );
        self.xpc_next_object
    }

    /// Drops every host callback so machines that call each other can be freed.
    pub(crate) fn release_handlers(&mut self) {
        self.entries.clear();
        self.fair_play_handler = None;
        self.mach_rpc_handler = None;
    }
}

/// Go's `newShimsWithOptions`: maps the service area, registers every service and installs
/// the dispatcher. The engine's data must be a fresh [`State`].
pub(crate) fn install(uc: &mut Uc, options: ShimOptions) -> Result<()> {
    uc.mem_map(SHIM_BASE, SHIM_SIZE, Prot::ALL)
        .uc()
        .context("map guest service area")?;
    super::shim_memory::register(uc)?;
    super::shim_platform::register(uc)?;
    add_aliases(
        uc,
        options.zero_return_aliases,
        &handler(super::shim_platform::return_zero),
    )
    .context("register profile guest services")?;
    let hook = uc
        .add_code_hook(
            SHIM_BASE,
            SHIM_BASE + SHIM_CODE_SIZE - 1,
            |uc, address, _| {
                dispatch(uc, address);
            },
        )
        .uc()
        .context("register guest service dispatcher")?;
    uc.get_data_mut().hook = Some(hook);
    Ok(())
}

/// Go's `shims.close`.
pub(crate) fn close(uc: &mut Uc) -> Result<()> {
    let Some(hook) = uc.get_data_mut().hook.take() else {
        return Ok(());
    };
    uc.remove_hook(hook)
        .uc()
        .context("close guest service hook")
}

pub(crate) fn handler(function: Service) -> Handler {
    Rc::new(function)
}

pub(crate) fn resolve(uc: &mut Uc, name: &str) -> Result<u64> {
    if let Some(&address) = uc.get_data().symbols.get(name) {
        return Ok(address);
    }
    let message = format!("guest called unsupported import {name}");
    add_function(uc, name, Rc::new(move |_| Err(Error::msg(message.clone()))))
}

pub(crate) fn add_aliases(uc: &mut Uc, names: &[&str], handler: &Handler) -> Result<()> {
    for name in names {
        add_function(uc, name, handler.clone())?;
    }
    Ok(())
}

pub(crate) fn add_function(uc: &mut Uc, name: &str, handler: Handler) -> Result<u64> {
    let state = uc.get_data();
    if let Some(&address) = state.symbols.get(name) {
        return Ok(address);
    }
    if state.code_cursor + SHIM_SLOT_SIZE > SHIM_BASE + SHIM_CODE_SIZE {
        return Err(Error::msg("guest service code area is full"));
    }
    let address = state.code_cursor;
    uc.get_data_mut().code_cursor += SHIM_SLOT_SIZE;
    uc.write_mem(address, &[0xc0, 0x03, 0x5f, 0xd6])
        .context("write guest service stub")?;
    let state = uc.get_data_mut();
    state.entries.insert(
        address,
        ShimEntry {
            name: name.into(),
            handler,
        },
    );
    state.symbols.insert(name.to_owned(), address);
    Ok(address)
}

pub(crate) fn add_data(uc: &mut Uc, name: &str, data: &[u8]) -> Result<u64> {
    let state = uc.get_data_mut();
    if let Some(&address) = state.symbols.get(name) {
        return Ok(address);
    }
    state.data_cursor = align(state.data_cursor, 8);
    let length = data.len() as u64;
    if state.data_cursor + length > SHIM_BASE + SHIM_SIZE {
        return Err(Error::msg("guest service data area is full"));
    }
    let address = state.data_cursor;
    state.data_cursor += length.max(8);
    uc.write_mem(address, data)
        .context("write guest service data")?;
    uc.get_data_mut().symbols.insert(name.to_owned(), address);
    Ok(address)
}

fn dispatch(uc: &mut Uc, address: u64) {
    let Some(entry) = uc.get_data().entries.get(&address).cloned() else {
        fail(
            uc,
            Error::msg(format!(
                "guest entered unknown service address {address:#x}"
            )),
        );
        return;
    };
    if let Err(error) = (entry.handler)(uc) {
        fail(uc, error.wrap(&*entry.name));
    }
}

/// Records the first failure and stops the guest.
pub(crate) fn fail(uc: &mut Uc, error: Error) {
    let state = uc.get_data_mut();
    if state.fault.is_none() {
        state.fault = Some(error);
    }
    if let Err(stop) = uc.emu_stop() {
        let state = uc.get_data_mut();
        if state.fault.is_none() {
            state.fault = Some(crate::engine::unicorn_error(stop));
        }
    }
}

pub(crate) fn checked_size(value: u64) -> Result<usize> {
    if value > MAX_GUEST_TRANSFER {
        return Err(Error::msg(format!(
            "guest transfer size {value} exceeds limit"
        )));
    }
    usize::try_from(value)
        .map_err(|_| Error::msg(format!("guest transfer size {value} exceeds limit")))
}

pub(crate) fn low32(value: u64) -> u32 {
    let bytes = value.to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Go's `unicorn.Engine` accessors and the shims' argument and memory helpers.
pub(crate) trait Guest {
    fn read_mem(&self, address: u64, size: u64) -> Result<Vec<u8>>;
    fn read_mem_into(&self, data: &mut [u8], address: u64) -> Result<()>;
    fn write_mem(&mut self, address: u64, data: &[u8]) -> Result<()>;
    fn map_mem(&mut self, address: u64, size: u64) -> Result<()>;
    fn reg(&self, register: i32) -> Result<u64>;
    fn set_reg(&mut self, register: i32, value: u64) -> Result<()>;
    fn reg128(&self, register: i32) -> Result<[u8; 16]>;
    fn set_reg128(&mut self, register: i32, value: [u8; 16]) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
}

impl<D> Guest for Unicorn<'_, D> {
    fn read_mem(&self, address: u64, size: u64) -> Result<Vec<u8>> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let size = usize::try_from(size)
            .map_err(|_| Error::msg(format!("guest read size {size} exceeds limit")))?;
        self.mem_read_as_vec(address, size).uc()
    }

    fn read_mem_into(&self, data: &mut [u8], address: u64) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.mem_read(address, data).uc()
    }

    fn write_mem(&mut self, address: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.mem_write(address, data).uc()
    }

    fn map_mem(&mut self, address: u64, size: u64) -> Result<()> {
        self.mem_map(address, size, Prot::ALL).uc()
    }

    fn reg(&self, register: i32) -> Result<u64> {
        self.reg_read(register).uc()
    }

    fn set_reg(&mut self, register: i32, value: u64) -> Result<()> {
        self.reg_write(register, value).uc()
    }

    fn reg128(&self, register: i32) -> Result<[u8; 16]> {
        let value = self.reg_read_long(register).uc()?;
        let mut result = [0; 16];
        let length = value.len().min(16);
        result[..length].copy_from_slice(&value[..length]);
        Ok(result)
    }

    fn set_reg128(&mut self, register: i32, value: [u8; 16]) -> Result<()> {
        self.reg_write_long(register, &value).uc()
    }

    fn stop(&mut self) -> Result<()> {
        self.emu_stop().uc()
    }
}

/// The shims' ABI helpers over the guest registers and memory.
pub(crate) trait Abi: Guest {
    fn argument(&self, index: usize) -> Result<u64> {
        if let Some(&register) = ARGUMENT_REGISTERS.get(index) {
            return self.reg(register).context("read guest argument register");
        }
        let stack = self.reg(REG_SP).context("read guest stack register")?;
        let data = self
            .read_mem(stack + (index as u64 - 8) * 8, 8)
            .context("read guest stack argument")?;
        Ok(le64(&data))
    }

    fn set_result(&mut self, value: u64) -> Result<()> {
        self.set_reg(REG_X0, value)
            .context("write guest result register")
    }

    fn read_u32(&self, address: u64) -> Result<u32> {
        let data = self.read_mem(address, 4).context("read guest uint32")?;
        Ok(le32(&data))
    }

    fn write_u32(&mut self, address: u64, value: u32) -> Result<()> {
        self.write_mem(address, &value.to_le_bytes())
            .context("write guest uint32")
    }

    fn read_u64(&self, address: u64) -> Result<u64> {
        let data = self.read_mem(address, 8).context("read guest uint64")?;
        Ok(le64(&data))
    }

    fn write_u64(&mut self, address: u64, value: u64) -> Result<()> {
        self.write_mem(address, &value.to_le_bytes())
            .context("write guest uint64")
    }

    fn read_c_string(&self, address: u64) -> Result<Vec<u8>> {
        const MAXIMUM: usize = 4096;
        let mut value = Vec::with_capacity(64);
        while value.len() < MAXIMUM {
            let item = self
                .read_mem(address + value.len() as u64, 1)
                .context("read guest string")?;
            if item[0] == 0 {
                return Ok(value);
            }
            value.push(item[0]);
        }
        Err(Error::msg(format!("guest string exceeds {MAXIMUM} bytes")))
    }
}

impl<D> Abi for Unicorn<'_, D> {}

pub(crate) fn le32(data: &[u8]) -> u32 {
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&data[..4]);
    u32::from_le_bytes(bytes)
}

pub(crate) fn le64(data: &[u8]) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&data[..8]);
    u64::from_le_bytes(bytes)
}
