//! Go `runtime.go`: guest memory, object table, message entry and the step dispatcher.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use unicorn_engine::{HookType, Prot, UcHookId, Unicorn};

use super::async_tasks::AsyncTask;
use super::compression::InflateStream;
use super::memory::{check_profile_access, group_pages};
use super::metadata::{CACHED_METADATA, RuntimeMetadata};
use super::network::NetworkState;
use super::profile_pages::{ProfileMemory, write_profile_pages};
use super::value::{Payload, Value};
use super::{Context as _, Error, Result, fail};
use crate::profile::{self, Class};

pub(crate) const HEAP_BASE: u64 = 0x4000_0000_0000;
pub(crate) const HEAP_SIZE: u64 = 64 << 20;
pub(crate) const STACK_BASE: u64 = 0x5000_0000_0000;
pub(crate) const STACK_SIZE: u64 = 8 << 20;
pub(crate) const RETURN_PC: u64 = 0x1_0000_0000;

const APPLE_MEDIA_SERVICES_SHA256: &str =
    "95f4558aa2c0ccd9f96ed479e336b630b46bf4b4e0baabc1c1ac8c7d55e21ab4";
const PLUGIN_SHA256: &str = "e33fff05a6e6593dd91e33fd103b5d3068b62a787c680df33b859f19b3acc4ea";
pub(crate) const RET: [u8; 4] = [0xc0, 0x03, 0x5f, 0xd6];

pub(crate) const REG_X29: i32 = 1;
pub(crate) const REG_LR: i32 = 2;
pub(crate) const REG_SP: i32 = 4;
pub(crate) const REG_D0: i32 = 40;
pub(crate) const REG_X0: i32 = 199;
pub(crate) const REG_X1: i32 = REG_X0 + 1;
pub(crate) const REG_X2: i32 = REG_X0 + 2;
#[allow(dead_code, reason = "network.rs uses it")]
pub(crate) const REG_X3: i32 = REG_X0 + 3;
pub(crate) const REG_X8: i32 = REG_X0 + 8;
pub(crate) const REG_X20: i32 = REG_X0 + 20;
pub(crate) const REG_X22: i32 = REG_X0 + 22;
pub(crate) const REG_PC: i32 = 260;

pub(crate) type Uc<'a> = Unicorn<'a, State>;
/// What runs when a guest callback returns (Go's `after func()`).
pub(crate) type After = Box<dyn FnOnce(&mut Uc) -> Result<()>>;
/// A host service at a guest address.
pub(crate) type HostFn = Rc<dyn Fn(&mut Uc) -> Result<()>>;
/// Go's `xpcSyncHandler`: `(service, request) -> reply`.
pub type XpcHandler = Box<dyn FnMut(&str, &[u8]) -> Result<Vec<u8>>>;

pub(crate) enum Callback {
    /// Runs once and is removed first (Go's callback resumes delete themselves).
    Once(After),
    Shared(HostFn),
}

/// Go's `Object`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Object {
    pub(crate) class: String,
    pub(crate) value: Payload,
    pub(crate) properties: HashMap<String, u64>,
}

impl Object {
    pub(crate) fn new(class: &str, value: Payload) -> Self {
        Self {
            class: class.to_owned(),
            value,
            properties: HashMap::new(),
        }
    }

    /// Go's `Properties[key]`, zero when absent.
    pub(crate) fn prop(&self, key: &str) -> u64 {
        self.properties.get(key).copied().unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code, reason = "Go records timers it never fires")]
pub(crate) struct Timer {
    pub(crate) at: u64,
    pub(crate) block: u64,
}

/// Go's `Runtime` fields: every host value the guest can reach.
pub struct State {
    pub(crate) meta: Arc<RuntimeMetadata>,
    pub(crate) classes: HashMap<String, Arc<Class>>,
    pub(crate) class_names: HashMap<u64, String>,
    pub(crate) methods: HashMap<String, u64>,
    pub(crate) selectors: HashMap<u64, String>,
    pub(crate) selector_addresses: HashMap<String, u64>,
    pub(crate) objects: HashMap<u64, Object>,
    pub(crate) pages: HashSet<u64>,
    pub(crate) cursor: u64,
    pub(crate) allocations: HashMap<u64, u64>,
    pub(crate) callbacks: HashMap<u64, Callback>,
    pub(crate) pending: Vec<u64>,
    pub(crate) pending_queues: HashMap<u64, u64>,
    pub(crate) current_queue: u64,
    pub(crate) selected_queue: u64,
    pub(crate) timers: Vec<Timer>,
    pub(crate) tasks: HashMap<u64, AsyncTask>,
    /// The running Swift task's handle (Go's `currentTask`).
    pub(crate) current_task: Option<u64>,
    pub(crate) task_local_frames: HashMap<u64, Vec<(u64, u64)>>,
    pub(crate) null_object: u64,
    pub(crate) associations: HashMap<u64, HashMap<u64, u64>>,
    pub(crate) preferences: HashMap<String, u64>,
    pub(crate) notification_center: u64,
    /// Observer records by their `HostObserver` object address.
    pub(crate) observers: BTreeSet<u64>,
    pub(crate) inflaters: HashMap<u64, InflateStream>,
    pub(crate) hardware_id: Vec<u8>,
    pub(crate) xpc_sync_handler: Option<XpcHandler>,
    pub(crate) code_ranges: Vec<[u64; 2]>,
    pub(crate) steps: u64,
    pub(crate) fault: Option<Error>,
    pub(crate) trace: Vec<String>,
    /// Strict mode: the pages guest and host accesses may touch, and the first violation.
    pub(crate) guard: Option<HashSet<u64>>,
    pub(crate) guard_error: Option<String>,
    #[allow(dead_code, reason = "network.rs owns it")]
    pub(crate) network: NetworkState,
}

impl State {
    pub(crate) fn new(meta: Arc<RuntimeMetadata>) -> Self {
        Self {
            meta,
            classes: HashMap::new(),
            class_names: HashMap::new(),
            methods: HashMap::new(),
            selectors: HashMap::new(),
            selector_addresses: HashMap::new(),
            objects: HashMap::new(),
            pages: HashSet::new(),
            cursor: 0,
            allocations: HashMap::new(),
            callbacks: HashMap::new(),
            pending: Vec::new(),
            pending_queues: HashMap::new(),
            current_queue: 0,
            selected_queue: 0,
            timers: Vec::new(),
            tasks: HashMap::new(),
            current_task: None,
            task_local_frames: HashMap::new(),
            null_object: 0,
            associations: HashMap::new(),
            preferences: HashMap::new(),
            notification_center: 0,
            observers: BTreeSet::new(),
            inflaters: HashMap::new(),
            hardware_id: Vec::new(),
            xpc_sync_handler: None,
            code_ranges: Vec::new(),
            steps: 0,
            fault: None,
            trace: Vec::new(),
            guard: None,
            guard_error: None,
            network: NetworkState::default(),
        }
    }

    pub(crate) fn profile(&self) -> &profile::Profile {
        &self.meta.profile
    }

    /// Go's `r.profile.Symbols[name]`.
    pub(crate) fn symbol(&self, name: &str) -> u64 {
        self.meta
            .profile
            .symbols
            .get(name)
            .copied()
            .unwrap_or_default()
    }

    /// Go's `r.profile.Bridges[name]`.
    pub(crate) fn bridge(&self, name: &str) -> u64 {
        self.meta
            .profile
            .bridges
            .get(name)
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn class(&self, name: &str) -> Option<&Arc<Class>> {
        self.classes.get(name)
    }
}

/// Go's `methodKey`: `-[Class selector]` or `+[Class selector]`.
pub(crate) fn method_key(class: &str, selector: &str, meta: bool) -> String {
    let prefix = if meta { '+' } else { '-' };
    format!("{prefix}[{class} {selector}]")
}

/// Go's `%v` of a `[]string`.
fn go_strings(items: &[String]) -> String {
    format!("[{}]", items.join(" "))
}

fn uc_error(error: unicorn_engine::uc_error) -> Error {
    Error::Sap(ipatool_sap::engine::unicorn_error(error))
}

/// Go's runtime text for a nil `*Object` dereference.
pub(crate) const NIL_DEREFERENCE: &str =
    "runtime error: invalid memory address or nil pointer dereference";

/// Go's text for a failed `value.(T)` assertion.
pub(crate) fn conversion(payload: &Payload, want: &str) -> Error {
    let have = match payload {
        Payload::Nil => "nil",
        other => other.go_type(),
    };
    Error::msg(format!(
        "interface conversion: interface {{}} is {have}, not {want}"
    ))
}

/// The guest-facing services every bridge file uses (Go's `Runtime` helper methods).
pub(crate) trait Guest {
    fn st(&mut self) -> &mut State;
    fn sti(&self) -> &State;
    fn reg(&self, register: i32) -> Result<u64>;
    fn set_reg(&mut self, register: i32, value: u64) -> Result<()>;
    fn arg(&self, index: usize) -> Result<u64>;
    fn set(&mut self, value: u64) -> Result<()>;
    fn read_mem(&self, address: u64, size: u64) -> Result<Vec<u8>>;
    fn write_mem(&mut self, address: u64, data: &[u8]) -> Result<()>;
    fn map_mem(&mut self, address: u64, size: u64) -> Result<()>;
    fn read64(&self, address: u64) -> Result<u64>;
    fn write64(&mut self, address: u64, value: u64) -> Result<()>;
    fn write32(&mut self, address: u64, value: u32) -> Result<()>;
    fn alloc(&mut self, size: u64) -> Result<u64>;
    fn cstring(&self, address: u64) -> Result<String>;
    fn cstring_bytes(&self, address: u64) -> Result<Vec<u8>>;
    fn selector(&mut self, value: &str) -> Result<u64>;
    fn new_object(&mut self, class: &str, value: Payload) -> Result<u64>;
    fn string(&self, address: u64) -> Result<String>;
    fn obj(&self, address: u64) -> Result<&Object>;
    fn obj_mut(&mut self, address: u64) -> Result<&mut Object>;
    fn lookup(&self, class: &str, selector: &str, meta: bool) -> u64;
    fn kind_of(&self, class: &str, target: &str) -> bool;
    fn class_address(&self, name: &str) -> u64;
    fn jump(&mut self, entry: u64, receiver: u64, selector: &str) -> Result<()>;
    fn dict(&self, address: u64) -> Result<&BTreeMap<String, u64>>;
    fn dict_mut(&mut self, address: u64) -> Result<&mut BTreeMap<String, u64>>;
    fn array(&self, address: u64) -> Result<&Vec<u64>>;
    fn data(&self, address: u64) -> Result<&Vec<u8>>;
}

impl Guest for Uc<'_> {
    fn st(&mut self) -> &mut State {
        self.get_data_mut()
    }

    fn sti(&self) -> &State {
        self.get_data()
    }

    fn reg(&self, register: i32) -> Result<u64> {
        self.reg_read(register).map_err(uc_error)
    }

    fn set_reg(&mut self, register: i32, value: u64) -> Result<()> {
        self.reg_write(register, value).map_err(uc_error)
    }

    fn arg(&self, index: usize) -> Result<u64> {
        if index >= 8 {
            let sp = self.reg(REG_SP)?;
            return self.read64(sp + (index as u64 - 8) * 8);
        }
        self.reg(REG_X0 + i32::try_from(index).unwrap_or_default())
    }

    fn set(&mut self, value: u64) -> Result<()> {
        self.set_reg(REG_X0, value)
    }

    fn read_mem(&self, address: u64, size: u64) -> Result<Vec<u8>> {
        if let Some(allowed) = &self.sti().guard {
            check_profile_access(allowed, address, size)?;
        }
        let size =
            usize::try_from(size).map_err(|_| Error::msg("guest read exceeds host memory"))?;
        self.mem_read_as_vec(address, size).map_err(uc_error)
    }

    /// Go's `Runtime.MemWrite`.
    fn write_mem(&mut self, address: u64, data: &[u8]) -> Result<()> {
        if let Some(allowed) = &self.sti().guard {
            check_profile_access(allowed, address, data.len() as u64)
                .context("write guest memory")?;
        }
        Unicorn::mem_write(self, address, data)
            .map_err(uc_error)
            .context("write guest memory")
    }

    /// Go's `Runtime.MemMap`: maps only the pages not mapped yet.
    fn map_mem(&mut self, address: u64, size: u64) -> Result<()> {
        let end = address + size;
        let overlap = (address..end)
            .step_by(4096)
            .any(|page| self.sti().pages.contains(&page));
        if !overlap {
            Unicorn::mem_map(self, address, size, Prot::ALL)
                .map_err(uc_error)
                .context("map guest memory")?;
            self.st().pages.extend((address..end).step_by(4096));
            return Ok(());
        }
        let mut page = address;
        while page < end {
            if self.sti().pages.contains(&page) {
                page += 4096;
                continue;
            }
            let mut stop = page + 4096;
            while stop < end && !self.sti().pages.contains(&stop) {
                stop += 4096;
            }
            Unicorn::mem_map(self, page, stop - page, Prot::ALL)
                .map_err(uc_error)
                .context("map guest memory")?;
            self.st().pages.extend((page..stop).step_by(4096));
            page = stop;
        }
        Ok(())
    }

    fn read64(&self, address: u64) -> Result<u64> {
        let bytes = self
            .read_mem(address, 8)
            .map_err(|error| Error::msg(format!("read guest pointer {address:#x}: {error}")))?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap_or_default()))
    }

    fn write64(&mut self, address: u64, value: u64) -> Result<()> {
        self.write_mem(address, &value.to_le_bytes())
    }

    fn write32(&mut self, address: u64, value: u32) -> Result<()> {
        self.write_mem(address, &value.to_le_bytes())
    }

    fn alloc(&mut self, size: u64) -> Result<u64> {
        if size > HEAP_SIZE {
            fail!("guest allocation exceeds heap budget");
        }
        let size = match (size + 15) & !15 {
            0 => 16,
            size => size,
        };
        let state = self.st();
        if size > HEAP_SIZE - state.cursor {
            fail!("guest heap exhausted");
        }
        let address = HEAP_BASE + state.cursor;
        state.cursor += size;
        state.allocations.insert(address, size);
        Ok(address)
    }

    fn cstring(&self, address: u64) -> Result<String> {
        if address != 0
            && let Some(value) = self.sti().selectors.get(&address)
        {
            return Ok(value.clone());
        }
        Ok(String::from_utf8_lossy(&self.cstring_bytes(address)?).into_owned())
    }

    /// Go's `cstring` as raw bytes (Go strings need not be UTF-8).
    fn cstring_bytes(&self, address: u64) -> Result<Vec<u8>> {
        if address == 0 {
            return Ok(Vec::new());
        }
        if let Some(value) = self.sti().selectors.get(&address) {
            return Ok(value.clone().into_bytes());
        }
        let mut bytes = Vec::new();
        let mut cursor = address;
        while bytes.len() < 16384 {
            // Read up to the page end: mappings and guard checks are page-granular, so this
            // fails exactly where Go's byte-at-a-time read does.
            let chunk = (4096 - cursor % 4096).min(16384 - bytes.len() as u64);
            let data = self
                .read_mem(cursor, chunk)
                .map_err(|error| Error::msg(format!("read C string {address:#x}: {error}")))?;
            if let Some(end) = data.iter().position(|&byte| byte == 0) {
                bytes.extend_from_slice(&data[..end]);
                return Ok(bytes);
            }
            bytes.extend_from_slice(&data);
            cursor += chunk;
        }
        fail!("unterminated guest string")
    }

    fn selector(&mut self, value: &str) -> Result<u64> {
        if let Some(&address) = self.sti().selector_addresses.get(value)
            && address != 0
        {
            return Ok(address);
        }
        let address = self.alloc(value.len() as u64 + 1)?;
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        self.write_mem(address, &bytes)?;
        let state = self.st();
        state.selector_addresses.insert(value.to_owned(), address);
        state.selectors.insert(address, value.to_owned());
        Ok(address)
    }

    /// Go's `New`.
    fn new_object(&mut self, class: &str, value: Payload) -> Result<u64> {
        let (size, isa) = match self.sti().class(class) {
            Some(class) => (class.size.max(16), Some(class.address)),
            None => (256, None),
        };
        let address = self.alloc(size)?;
        self.st().objects.insert(address, Object::new(class, value));
        if let Some(isa) = isa {
            self.write64(address, isa)?;
        }
        Ok(address)
    }

    /// Go's `String`.
    fn string(&self, address: u64) -> Result<String> {
        if address == 0 {
            return Ok(String::new());
        }
        let Some(object) = self.sti().objects.get(&address) else {
            fail!("unknown string object {address:#x}");
        };
        match &object.value {
            Payload::String(text) => Ok(text.clone()),
            _ => fail!("object {} is not a string", object.class),
        }
    }

    fn obj(&self, address: u64) -> Result<&Object> {
        self.sti()
            .objects
            .get(&address)
            .ok_or_else(|| Error::msg(NIL_DEREFERENCE))
    }

    fn obj_mut(&mut self, address: u64) -> Result<&mut Object> {
        self.st()
            .objects
            .get_mut(&address)
            .ok_or_else(|| Error::msg(NIL_DEREFERENCE))
    }

    fn lookup(&self, class: &str, selector: &str, meta: bool) -> u64 {
        let state = self.sti();
        let mut class = class.to_owned();
        while !class.is_empty() {
            if let Some(&address) = state.methods.get(&method_key(&class, selector, meta))
                && address != 0
            {
                return address;
            }
            match state.class(&class) {
                Some(found) => class.clone_from(&found.superclass),
                None => break,
            }
        }
        0
    }

    /// Go's `kindOf`.
    fn kind_of(&self, class: &str, target: &str) -> bool {
        if class == target || target == "NSObject" {
            return true;
        }
        let state = self.sti();
        let mut class = class;
        while !class.is_empty() {
            let Some(found) = state.class(class) else {
                return false;
            };
            if found.superclass == target {
                return true;
            }
            class = &found.superclass;
        }
        false
    }

    fn class_address(&self, name: &str) -> u64 {
        self.sti().class(name).map_or(0, |class| class.address)
    }

    fn jump(&mut self, entry: u64, receiver: u64, selector: &str) -> Result<()> {
        let selector = self.selector(selector)?;
        self.set_reg(REG_X0, receiver)?;
        self.set_reg(REG_X1, selector)?;
        self.set_reg(REG_PC, entry)
    }

    fn dict(&self, address: u64) -> Result<&BTreeMap<String, u64>> {
        match &self.obj(address)?.value {
            Payload::Dict(map) => Ok(map),
            other => Err(conversion(other, "map[string]uint64")),
        }
    }

    fn dict_mut(&mut self, address: u64) -> Result<&mut BTreeMap<String, u64>> {
        match &mut self.obj_mut(address)?.value {
            Payload::Dict(map) => Ok(map),
            other => Err(conversion(other, "map[string]uint64")),
        }
    }

    fn array(&self, address: u64) -> Result<&Vec<u64>> {
        match &self.obj(address)?.value {
            Payload::Array(items) => Ok(items),
            other => Err(conversion(other, "[]uint64")),
        }
    }

    fn data(&self, address: u64) -> Result<&Vec<u8>> {
        match &self.obj(address)?.value {
            Payload::Data(bytes) => Ok(bytes),
            other => Err(conversion(other, "[]uint8")),
        }
    }
}

/// Loads Mach-O images through the runtime's page tracking.
struct GuestMemory<'a, 'b>(&'a mut Uc<'b>);

impl ipatool_sap::machimage::Memory for GuestMemory<'_, '_> {
    fn mem_map(&mut self, address: u64, size: u64) -> ipatool_sap::Result<()> {
        self.0
            .map_mem(address, size)
            .map_err(|error| ipatool_sap::Error::msg(error.to_string()))
    }

    fn mem_write(&mut self, address: u64, data: &[u8]) -> ipatool_sap::Result<()> {
        self.0
            .write_mem(address, data)
            .map_err(|error| ipatool_sap::Error::msg(error.to_string()))
    }
}

impl ProfileMemory for Uc<'_> {
    fn write(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.write_mem(address, data)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// Go's `Runtime`: one `AppleMediaServices` guest. Guest objects live until it is dropped.
pub struct Runtime {
    pub(crate) uc: Uc<'static>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Runtime").finish_non_exhaustive()
    }
}

impl Runtime {
    /// Go's `Open`.
    pub fn open(image: &[u8], prepared: &profile::Data) -> Result<Self> {
        open(image, prepared, false)
    }

    /// Go's `Invoke`: sends `selector` to `object` and runs the guest until it returns.
    pub fn invoke(&mut self, object: u64, selector: &str, args: &[u64]) -> Result<u64> {
        invoke(&mut self.uc, object, selector, args)
    }

    /// Go's `Export`: copies a Foundation value out of the guest without guest pointers.
    pub fn export(&self, address: u64) -> Result<Value> {
        export(&self.uc, address, &mut HashSet::new(), 0)
    }

    /// Go's `New`.
    pub fn new_object(&mut self, class: &str, value: Payload) -> Result<u64> {
        self.uc
            .new_object(class, value)
            .map_err(|error| error.wrap("guest setup"))
    }

    /// Go's `String`.
    pub fn string(&self, address: u64) -> Result<String> {
        self.uc.string(address)
    }

    #[must_use]
    pub fn class_address(&self, name: &str) -> u64 {
        self.uc.class_address(name)
    }

    /// Go's `SetIvar`.
    pub fn set_ivar(&mut self, object: u64, name: &str, value: u64) -> Result<()> {
        set_ivar(&mut self.uc, object, name, value)
    }
}

pub(crate) fn set_ivar(uc: &mut Uc, object: u64, name: &str, value: u64) -> Result<()> {
    let Some(found) = uc.sti().objects.get(&object) else {
        fail!("unknown guest object");
    };
    let class_name = found.class.clone();
    let mut class = class_name.clone();
    while !class.is_empty() {
        let Some(found) = uc.sti().class(&class).cloned() else {
            break;
        };
        if let Some(&offset) = found.ivars.as_ref().and_then(|ivars| ivars.get(name)) {
            return uc.write64(object + offset, value);
        }
        class.clone_from(&found.superclass);
    }
    fail!("unknown ivar {class_name}.{name}")
}

/// Go's `open`; `strict` guards accesses against the exported page set. Validation and
/// production use the same grouped mappings to stay within Unicorn section limits.
pub(crate) fn open(image: &[u8], prepared: &profile::Data, strict: bool) -> Result<Runtime> {
    if hex(&Sha256::digest(image)) != APPLE_MEDIA_SERVICES_SHA256 {
        fail!("AppleMediaServices does not match the pinned 26A434 image");
    }
    let metadata = CACHED_METADATA
        .load(&prepared.metadata)
        .context("prepare store runtime")?;
    let mut inputs: Vec<(&str, &[u8])> = vec![("AppleMediaServices", image)];
    if !metadata.profile.plugin_uuid.is_empty() {
        if metadata.profile.plugin_uuid != profile::PLUGIN_UUID {
            fail!("unsupported authentication plugin UUID");
        }
        if hex(&Sha256::digest(&prepared.plugin)) != PLUGIN_SHA256 {
            fail!("authentication plugin does not match the pinned image");
        }
        inputs.push(("AMSAccountAuthenticationPlugin", &prepared.plugin));
    }
    let state = metadata.new_state();
    let mut uc = ipatool_sap::engine::new(state).context("prepare store runtime")?;
    let result = prepare(&mut uc, &metadata, &inputs, &prepared.pages, strict);
    result.context("prepare store runtime")?;
    Ok(Runtime { uc })
}

fn prepare(
    uc: &mut Uc,
    metadata: &RuntimeMetadata,
    inputs: &[(&str, &[u8])],
    page_data: &[u8],
    strict: bool,
) -> Result<()> {
    for (address, size) in [
        (HEAP_BASE, HEAP_SIZE),
        (STACK_BASE, STACK_SIZE),
        (RETURN_PC, 4096),
    ] {
        uc.map_mem(address, size)?;
    }
    let mut images = Vec::with_capacity(inputs.len());
    for &(name, data) in inputs {
        let mut image = ipatool_sap::machimage::Image::open_shared(name, data)?;
        image.prepare_prelinked(&[], &mut |symbol| {
            Err(ipatool_sap::Error::msg(format!(
                "unexpected relocation {symbol}"
            )))
        })?;
        image.load(&mut GuestMemory(uc))?;
        images.push(image);
    }
    let profile = &metadata.profile;
    let mut addresses: Vec<u64> = Vec::with_capacity(profile.pages.len());
    for page in &profile.pages {
        if !uc.sti().pages.contains(&page.address) {
            addresses.push(page.address);
        }
    }
    for call in &profile.calls {
        let page = call.address & !4095;
        if !uc.sti().pages.contains(&page) {
            addresses.push(page);
        }
    }
    if strict {
        let mut allowed = uc.sti().pages.clone();
        allowed.extend(addresses.iter().copied());
        install_guard(uc, allowed).context("guard Store memory")?;
    }
    for region in group_pages(&mut addresses)? {
        uc.map_mem(region.address, region.size)?;
    }
    write_profile_pages(uc, &profile.pages, page_data)?;
    for call in &profile.calls {
        let page = call.address & !4095;
        if !uc.sti().pages.contains(&page) {
            uc.map_mem(page, 4096)?;
        }
        uc.write_mem(call.address, &RET)?;
    }
    // Non-fragile ivar offsets are fixed by the Objective-C loader on macOS.
    for image in &images {
        let ranges = image.instruction_ranges();
        uc.st().code_ranges.extend(ranges);
        for symbol in image.symbols() {
            let Some(name) = symbol.name.strip_prefix("_OBJC_IVAR_$_") else {
                continue;
            };
            let Some((class, ivar)) = name.split_once('.') else {
                continue;
            };
            let offset = uc
                .sti()
                .class(class)
                .and_then(|class| class.ivars.as_ref()?.get(ivar).copied());
            if let Some(offset) = offset {
                #[allow(clippy::cast_possible_truncation, reason = "Go's uint32(offset)")]
                uc.write_mem(symbol.value, &(offset as u32).to_le_bytes())?;
            }
        }
    }
    for patch in &profile.metadata_patches {
        uc.write_mem(patch.address, &patch.data)?;
    }
    uc.add_code_hook(0, u64::MAX, |uc, address, _| step(uc, address))
        .map_err(uc_error)?;
    Ok(())
}

/// Go's `GuardMemory`: guest reads, writes and instructions, and host accesses, must stay on
/// `allowed` pages; a rejected guest access permanently fails execution.
fn install_guard(uc: &mut Uc, allowed: HashSet<u64>) -> Result<()> {
    fn reject(uc: &mut Uc, address: u64, size: u64) {
        let Some(allowed) = &uc.get_data().guard else {
            return;
        };
        if let Err(error) = check_profile_access(allowed, address, size) {
            let state = uc.get_data_mut();
            if state.guard_error.is_none() {
                state.guard_error = Some(error.to_string());
            }
            let _ = uc.emu_stop();
        }
    }
    uc.st().guard = Some(allowed);
    let hooks: [std::result::Result<UcHookId, _>; 2] = [
        uc.add_mem_hook(HookType::MEM_VALID, 1, 0, |uc, _, address, size, _| {
            reject(uc, address, size as u64);
            true
        }),
        uc.add_code_hook(1, 0, |uc, address, size| {
            reject(uc, address, u64::from(size));
        }),
    ];
    for hook in hooks {
        hook.map_err(uc_error)?;
    }
    Ok(())
}

fn receiver_class(uc: &Uc, object: u64, selector: &str) -> Result<(String, bool)> {
    let state = uc.sti();
    if let Some(class) = state.class_names.get(&object) {
        return Ok((class.clone(), true));
    }
    match state.objects.get(&object) {
        Some(found) => Ok((found.class.clone(), false)),
        None => fail!("unknown receiver {object:#x} for {selector}"),
    }
}

pub(crate) fn invoke(uc: &mut Uc, object: u64, selector: &str, args: &[u64]) -> Result<u64> {
    let (class, meta) = receiver_class(uc, object, selector)?;
    let setup = |uc: &mut Uc| -> Result<(u64, Vec<u64>)> {
        let mut entry = uc.lookup(&class, selector, meta);
        if entry == 0 {
            let selector = selector.to_owned();
            let block = host_block(
                uc,
                Rc::new(move |uc: &mut Uc| super::bridge::message(uc, object, &selector, false)),
            )?;
            entry = uc.read64(block + 16)?;
        }
        let mut arguments = vec![object, uc.selector(selector)?];
        arguments.extend_from_slice(args);
        Ok((entry, arguments))
    };
    let (entry, arguments) = setup(uc).map_err(|error| error.wrap("guest setup"))?;
    call(uc, entry, &arguments)
}

pub(crate) fn call(uc: &mut Uc, entry: u64, arguments: &[u64]) -> Result<u64> {
    for index in 0..8 {
        let value = arguments.get(index).copied().unwrap_or_default();
        uc.set_reg(REG_X0 + i32::try_from(index).unwrap_or_default(), value)
            .context("write guest register")?;
    }
    let sp = STACK_BASE + STACK_SIZE - 4096;
    for (index, &value) in arguments.iter().enumerate().skip(8) {
        uc.write64(sp + (index as u64 - 8) * 8, value)
            .map_err(|error| error.wrap("guest setup"))?;
    }
    uc.set_reg(REG_SP, sp)?;
    uc.set_reg(REG_LR, RETURN_PC)?;
    uc.st().fault = None;
    let native = ipatool_sap::engine::start_bounded(
        uc,
        entry,
        RETURN_PC,
        Duration::from_secs(120),
        20_000_000,
    );
    if let Some(fault) = uc.st().fault.take() {
        return Err(fault);
    }
    let native = native.err().map(Error::Sap);
    let error = match (uc.sti().guard_error.clone(), native) {
        (Some(guard), None) => Some(Error::msg(guard)),
        (Some(guard), Some(native)) => Some(Error::Joined(vec![Error::msg(guard), native])),
        (None, native) => native,
    };
    let pc = uc.reg(REG_PC)?;
    if let Some(error) = error {
        let lr = uc.reg(REG_LR)?;
        let trace = go_strings(&uc.sti().trace);
        fail!("guest stopped at {pc:#x} from {lr:#x}: {error}; recent={trace}");
    }
    if pc != RETURN_PC {
        fail!("instruction budget exhausted at {pc:#x}");
    }
    uc.reg(REG_X0).context("read guest return register")
}

fn step(uc: &mut Uc, address: u64) {
    if let Err(error) = dispatch(uc, address) {
        let trace = go_strings(&uc.sti().trace);
        uc.st().fault = Some(Error::msg(format!(
            "guest bridge at {address:#x}: runtime: {error}; recent={trace}"
        )));
        let _ = uc.emu_stop();
    }
}

fn dispatch(uc: &mut Uc, address: u64) -> Result<()> {
    match uc.st().callbacks.get(&address) {
        Some(Callback::Shared(callback)) => {
            let callback = Rc::clone(callback);
            return callback(uc);
        }
        Some(Callback::Once(_)) => {
            if let Some(Callback::Once(callback)) = uc.st().callbacks.remove(&address) {
                return callback(uc);
            }
        }
        None => {}
    }
    let state = uc.st();
    if let Some(name) = state.meta.entries.get(&address) {
        let name = name.clone();
        state.trace.push(name);
        if state.trace.len() > 12 {
            let excess = state.trace.len() - 12;
            state.trace.drain(..excess);
        }
    }
    let Some(found) = state.meta.calls.get(&address) else {
        if !state
            .code_ranges
            .iter()
            .any(|range| address >= range[0] && address < range[1])
        {
            fail!("execution outside the pinned image at {address:#x}");
        }
        state.steps += 1;
        return Ok(());
    };
    if found.selector.is_empty() {
        let name = found.symbol.trim_start_matches('_').to_owned();
        super::bridge::symbol(uc, &name)
    } else {
        let selector = found.selector.clone();
        let receiver = uc.arg(0)?;
        super::bridge::message(uc, receiver, &selector, false)
    }
}

pub(crate) fn export(
    uc: &Uc,
    address: u64,
    active: &mut HashSet<u64>,
    depth: usize,
) -> Result<Value> {
    if address == 0 {
        return Ok(Value::Nil);
    }
    if depth > 32 || active.contains(&address) {
        fail!("cyclic or excessively nested guest value");
    }
    let Some(found) = uc.sti().objects.get(&address) else {
        fail!("unknown exported object {address:#x}");
    };
    active.insert(address);
    let result = match &found.value {
        Payload::Dict(map) => {
            let mut result = BTreeMap::new();
            for (key, &item) in map {
                result.insert(key.clone(), export(uc, item, active, depth + 1)?);
            }
            Value::Dictionary(result)
        }
        Payload::Array(items) => Value::Array(
            items
                .iter()
                .map(|&item| export(uc, item, active, depth + 1))
                .collect::<Result<_>>()?,
        ),
        Payload::Nil => Value::Nil,
        Payload::String(text) => Value::String(text.clone()),
        Payload::Int(value) => Value::Integer(*value),
        Payload::Bool(value) => Value::Boolean(*value),
        Payload::Float(value) => Value::Real(*value),
        Payload::Data(bytes) => Value::Data(bytes.clone()),
        Payload::Date(date) => Value::Date(*date),
        other => Value::Other(other.go_type()),
    };
    active.remove(&address);
    Ok(result)
}

/// Go's `hostBlock`: a global block whose invoke function runs `callback`.
pub(crate) fn host_block(uc: &mut Uc, callback: HostFn) -> Result<u64> {
    let function = uc.alloc(16)?;
    uc.write_mem(function, &RET)?;
    uc.st()
        .callbacks
        .insert(function, Callback::Shared(callback));
    let descriptor = uc.alloc(16)?;
    uc.write64(descriptor + 8, 32)?;
    let block = uc.alloc(32)?;
    uc.write64(block + 8, 1 << 28)?;
    uc.write64(block + 16, function)?;
    uc.write64(block + 24, descriptor)?;
    Ok(block)
}

pub(crate) fn queue_pending(uc: &mut Uc, block: u64, queue: u64) {
    let state = uc.st();
    state.pending_queues.insert(block, queue);
    state.pending.push(block);
}

/// Go's `nextPending`: the first block whose queue is not suspended, or 0.
pub(crate) fn next_pending(uc: &mut Uc) -> u64 {
    let state = uc.st();
    let position = state.pending.iter().position(|block| {
        let queue = state.pending_queues.get(block).copied().unwrap_or_default();
        state
            .objects
            .get(&queue)
            .is_none_or(|queue| queue.prop("suspendCount") == 0)
    });
    let Some(index) = position else {
        return 0;
    };
    let block = state.pending.remove(index);
    state.selected_queue = state.pending_queues.remove(&block).unwrap_or_default();
    block
}

pub(crate) fn pending_callback(uc: &mut Uc, block: u64, after: After) -> Result<()> {
    let state = uc.st();
    let previous = state.current_queue;
    state.current_queue = state.selected_queue;
    super::callbacks::callback(
        uc,
        block,
        &[],
        Box::new(move |uc| {
            uc.st().current_queue = previous;
            after(uc)
        }),
    )
}

pub(crate) fn queue_is_within(uc: &Uc, queue: u64, target: u64) -> Result<bool> {
    let mut queue = queue;
    for _ in 0..32 {
        if queue == target {
            return Ok(true);
        }
        let Some(found) = uc.sti().objects.get(&queue) else {
            return Ok(false);
        };
        queue = found.prop("targetQueue");
        if queue == 0 {
            return Ok(false);
        }
    }
    fail!("cyclic dispatch queue targets")
}

#[cfg(test)]
pub(crate) fn test_engine() -> Uc<'static> {
    ipatool_sap::engine::new(State::new(Arc::default())).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn still_rejects_execution_in_padding_outside_the_pinned_code() {
        let mut uc = test_engine();
        uc.st().code_ranges = vec![[0x1000, 0x2000]];
        step(&mut uc, 0x3000);
        let fault = uc.st().fault.take().unwrap().to_string();
        assert!(
            fault.contains("execution outside the pinned image"),
            "{fault}"
        );
    }
}
