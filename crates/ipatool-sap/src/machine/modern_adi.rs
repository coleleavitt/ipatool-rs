//! The ADI implementation of the pinned macOS 27 `commerce`, `adid`, `fairplayd` and `FairPlay`
//! kernel extension. Apple Store otp/1.5 provisioning needs this generation. Port of Go
//! `modern_adi.go`.

use std::cell::Cell;
use std::rc::Rc;

use super::shim_platform::{ADI_STATE_PATH, FAKE_HANDLE};
use super::shims::{Guest, ShimOptions, le32};
use super::storeagent::{
    STORE_AGENT_BASE, STORE_AGENT_GLOBAL_CLOSE, STORE_AGENT_GLOBAL_INIT, STORE_AGENT_SC_INFO_PATH,
    commerce_runtime,
};
use super::{ImageSpec, Machine, RuntimeOptions, hardware_block, open_runtime, status_code};
use crate::assets::Bundle;
use crate::error::{Context, Error, Result};

const START_ENTRY: u64 = STORE_AGENT_BASE + 0x001b_4400;
const END_ENTRY: u64 = STORE_AGENT_BASE + 0x001d_5124;
const OTP_ENTRY: u64 = STORE_AGENT_BASE + 0x001d_ab04;
const KEY_BAG_ENTRY: u64 = STORE_AGENT_BASE + 0x0016_e0f8;
const ADID_BASE: u64 = 0x0000_2000_0000_0000;
const ADID_ENTRY: u64 = ADID_BASE + 0x0001_1180;
const FAIRPLAYD_BASE: u64 = 0x0000_6000_0000_0000;
const FAIRPLAYD_ENTRY: u64 = FAIRPLAYD_BASE + 0x0033_2040;
// The prelinked image keeps its original 0x510 page offset so its ADRP references retain the
// same slide as its data segments.
const KEXT_BASE: u64 = 0x0000_7000_0000_0510;
const KEXT_START: u64 = KEXT_BASE + 0x02bc_a984;
const KEXT_SETUP: u64 = KEXT_BASE + 0x02c6_7e50;
const KEXT_INIT: u64 = KEXT_BASE + 0x02bc_a714;
const KEXT_ENTRY: u64 = KEXT_BASE + 0x02cf_978c;
const KEXT_KERNEL_BASE: u64 = 0xffff_fe00_0700_4000;

/// Where `FairPlay` `IOKit` calls from any of the machines are answered.
#[derive(Clone)]
struct FairPlayRoute {
    fairplay: Machine,
    kernel: Option<Machine>,
    kernel_context: Rc<Cell<u64>>,
}

struct Machines {
    runtime: Machine,
    daemon: Machine,
    fairplay: Machine,
    kernel: Option<Machine>,
    store_global: u32,
    daemon_handler: u64,
}

/// Go's `ModernADI`.
pub struct ModernAdi {
    machines: Option<Machines>,
}

/// Go's `fmt.Errorf("context: %w", err)`, including its text for a nil `err`.
fn wrap_optional(context: &str, error: Option<Error>) -> Error {
    match error {
        Some(error) => error.wrap(context),
        None => Error::msg(format!("{context}: %!w(<nil>)")),
    }
}

impl ModernAdi {
    pub fn open(bundle: &Bundle, identifier: &[u8], state: &[u8]) -> Result<Self> {
        let identifier: [u8; 16] = identifier
            .try_into()
            .map_err(|_| Error::msg("modern ADI host UUID must be 16 bytes"))?;
        let (mut runtime, _) = open_runtime(bundle, commerce_runtime(bundle))?;
        let daemon_options = ShimOptions {
            zero_return_aliases: &[],
            pause_run_loop: true,
        };
        let (mut daemon, _) = open_runtime(
            bundle,
            RuntimeOptions {
                extra_images: vec![ImageSpec::new("adid", &bundle.adid, ADID_BASE)],
                shims: daemon_options,
            },
        )
        .context("open modern ADI daemon")?;
        let (mut fairplay, _) = open_runtime(
            bundle,
            RuntimeOptions {
                extra_images: vec![ImageSpec::new(
                    "fairplayd",
                    &bundle.fairplayd,
                    FAIRPLAYD_BASE,
                )],
                shims: daemon_options,
            },
        )
        .context("open modern FairPlay daemon")?;
        runtime.state_mut().host_uuid = identifier;
        daemon.state_mut().host_uuid = identifier;
        fairplay.state_mut().host_uuid = identifier;
        if !state.is_empty() {
            daemon
                .state_mut()
                .adi_files
                .insert(ADI_STATE_PATH.to_owned(), state.to_vec());
        }
        let mut kernel = None;
        if !bundle.fairplay_kext.is_empty() {
            let (mut machine, _) = open_runtime(
                bundle,
                RuntimeOptions {
                    extra_images: vec![ImageSpec {
                        kernel_base: KEXT_KERNEL_BASE,
                        ..ImageSpec::new("FairPlayIOKit", &bundle.fairplay_kext, KEXT_BASE)
                    }],
                    shims: ShimOptions::default(),
                },
            )
            .context("open modern FairPlay kernel client")?;
            machine.state_mut().host_uuid = identifier;
            kernel = Some(machine);
        }

        let route = FairPlayRoute {
            fairplay: fairplay.clone(),
            kernel: kernel.clone(),
            kernel_context: Rc::new(Cell::new(0)),
        };
        let mut adi = Self {
            machines: Some(Machines {
                runtime,
                daemon,
                fairplay,
                kernel,
                store_global: 0,
                daemon_handler: 0,
            }),
        };
        let machines = adi.machines()?;
        if let Some(kernel) = machines.kernel.as_mut() {
            let rpc = route.fairplay.clone();
            kernel.state_mut().mach_rpc_handler = Some(Rc::new(move |request, receive_size| {
                handle_fair_play_mach_rpc(&mut rpc.clone(), request, receive_size)
            }));
            let context = initialize_fair_play_kernel(kernel)?;
            route.kernel_context.set(context);
        }
        let handler: super::shims::FairPlayHandler = {
            let route = route.clone();
            Rc::new(move |input, indirect, output_size| {
                handle_fair_play_message(&mut route.clone(), input, indirect, output_size)
            })
        };
        machines.fairplay.state_mut().fair_play_handler = Some(handler.clone());
        machines
            .fairplay
            .invoke(FAIRPLAYD_ENTRY, &[0, 0, 0])
            .context("initialize modern FairPlay daemon")?;
        machines.daemon.state_mut().fair_play_handler = Some(handler.clone());
        machines.runtime.state_mut().fair_play_handler = Some(handler);
        machines.initialize_daemon()?;
        Ok(adi)
    }

    fn machines(&mut self) -> Result<&mut Machines> {
        self.machines
            .as_mut()
            .ok_or_else(|| Error::msg("modern ADI runtime is unavailable"))
    }

    pub fn import_key_bag(&mut self, hardware_id: &[u8], key_bag: &[u8]) -> Result<()> {
        let machines = self.machines()?;
        let hardware = hardware_block(hardware_id)?;
        let runtime = &mut machines.runtime;
        runtime.begin_call();
        let result = (|| {
            let hardware_address = runtime.scratch(&hardware, hardware.len() as u64)?;
            let mut path = STORE_AGENT_SC_INFO_PATH.as_bytes().to_vec();
            path.push(0);
            let path_address = runtime.scratch(&path, path.len() as u64)?;
            let context_field = runtime.scratch(&[], 4)?;
            let status = runtime
                .invoke(
                    STORE_AGENT_GLOBAL_INIT,
                    &[0, hardware_address, path_address, context_field],
                )
                .context("initialize FairPlay keybag context")?;
            let status = status_code(status);
            if status != 0 {
                return Err(Error::msg(format!(
                    "initialize FairPlay keybag context: status {status}"
                )));
            }
            let global = runtime.read_u32(context_field)?;
            if global == 0 {
                return Err(Error::msg(
                    "initialize FairPlay keybag context: null context",
                ));
            }
            let key_bag_address = runtime.scratch(key_bag, key_bag.len() as u64)?;
            let status = runtime
                .invoke(
                    KEY_BAG_ENTRY,
                    &[u64::from(global), key_bag_address, key_bag.len() as u64],
                )
                .context("import FairPlay keybag")?;
            let status = status_code(status);
            if status != 0 {
                return Err(Error::msg(format!(
                    "import FairPlay keybag: status {status}"
                )));
            }
            Ok(global)
        })();
        runtime.clear_scratch();
        machines.store_global = result?;
        Ok(())
    }

    /// Sends the opaque payload `AppleMediaServices` addresses to `com.apple.adid` and returns
    /// the emulated service's reply.
    pub fn exchange_xpc(&mut self, request: &[u8]) -> Result<Vec<u8>> {
        let machines = self
            .machines
            .as_mut()
            .filter(|machines| machines.daemon_handler != 0)
            .ok_or_else(|| Error::msg("modern ADI daemon is unavailable"))?;
        if request.is_empty() || request.len() > 1 << 20 {
            return Err(Error::msg("modern ADI XPC request is invalid"));
        }
        let daemon = machines.daemon.state_mut();
        daemon.xpc_reply.clear();
        daemon.xpc_replied = false;
        let object = daemon.new_xpc_object(Some(request));
        let handler = machines.daemon_handler;
        machines
            .invoke_daemon_block(handler, object)
            .context("run modern ADI XPC request")?;
        let daemon = machines.daemon.state();
        if !daemon.xpc_replied || daemon.xpc_reply.is_empty() {
            return Err(Error::msg("modern ADI XPC service returned no reply"));
        }
        Ok(daemon.xpc_reply.clone())
    }

    pub fn start_provisioning(&mut self, dsid: u64, spim: &[u8]) -> Result<(Vec<u8>, u32)> {
        let machines = self.machines()?;
        machines.runtime.begin_call();
        let result = (|| {
            let runtime = &mut machines.runtime;
            let input = runtime.scratch(spim, spim.len() as u64)?;
            let pointer_field = runtime.scratch(&[], 8)?;
            let length_field = runtime.scratch(&[], 4)?;
            let session_field = runtime.scratch(&[], 4)?;
            let status = machines.invoke(
                START_ENTRY,
                &[
                    dsid,
                    input,
                    spim.len() as u64,
                    pointer_field,
                    length_field,
                    session_field,
                ],
            )?;
            let status = status_code(status);
            if status != 0 {
                return Err(Error::msg(format!(
                    "start modern ADI provisioning: status {status}"
                )));
            }
            let output = machines.read_output(pointer_field, length_field)?;
            let session = machines
                .runtime
                .uc
                .read_mem(session_field, 4)
                .context("read modern ADI provisioning session")?;
            Ok((output, le32(&session)))
        })();
        machines.runtime.clear_scratch();
        result
    }

    pub fn end_provisioning(
        &mut self,
        session: u32,
        setting_info: &[u8],
        transport_key: &[u8],
    ) -> Result<()> {
        let machines = self.machines()?;
        machines.runtime.begin_call();
        let result = (|| {
            let runtime = &mut machines.runtime;
            let setting_address = runtime.scratch(setting_info, setting_info.len() as u64)?;
            let key_address = runtime.scratch(transport_key, transport_key.len() as u64)?;
            let status = machines.invoke(
                END_ENTRY,
                &[
                    u64::from(session),
                    setting_address,
                    setting_info.len() as u64,
                    key_address,
                    transport_key.len() as u64,
                ],
            )?;
            let status = status_code(status);
            if status != 0 {
                return Err(Error::msg(format!(
                    "finish modern ADI provisioning: status {status}"
                )));
            }
            Ok(())
        })();
        machines.runtime.clear_scratch();
        result
    }

    /// Returns the machine identifier and one-time password for `dsid`.
    pub fn request_otp(&mut self, dsid: u64) -> Result<(Vec<u8>, Vec<u8>)> {
        let machines = self.machines()?;
        machines.runtime.begin_call();
        let result = (|| {
            let runtime = &mut machines.runtime;
            let mid_pointer = runtime.scratch(&[], 8)?;
            let mid_length = runtime.scratch(&[], 4)?;
            let otp_pointer = runtime.scratch(&[], 8)?;
            let otp_length = runtime.scratch(&[], 4)?;
            let status = machines.invoke(
                OTP_ENTRY,
                &[dsid, mid_pointer, mid_length, otp_pointer, otp_length],
            )?;
            let status = status_code(status);
            if status != 0 {
                return Err(Error::msg(format!(
                    "request modern ADI OTP: status {status}"
                )));
            }
            let mid = machines.read_output(mid_pointer, mid_length)?;
            let otp = machines.read_output(otp_pointer, otp_length)?;
            Ok((mid, otp))
        })();
        machines.runtime.clear_scratch();
        result
    }

    /// The daemon's persistent `adi.pb`.
    pub fn state(&mut self) -> Result<Vec<u8>> {
        let machines = self.machines()?;
        let files = &machines.daemon.state().adi_files;
        if let Some(state) = files.get(ADI_STATE_PATH).filter(|state| !state.is_empty()) {
            return Ok(state.clone());
        }
        files
            .iter()
            .find(|(path, state)| {
                std::path::Path::new(path).file_name() == Some("adi.pb".as_ref())
                    && !state.is_empty()
            })
            .map(|(_, state)| state.clone())
            .ok_or_else(|| Error::msg("modern ADI state was not created"))
    }

    pub fn close(&mut self) -> Result<()> {
        let Some(mut machines) = self.machines.take() else {
            return Ok(());
        };
        let mut errors = Vec::new();
        if machines.store_global != 0 {
            match machines.runtime.invoke(
                STORE_AGENT_GLOBAL_CLOSE,
                &[u64::from(machines.store_global)],
            ) {
                Err(error) => errors.push(Some(error.wrap("close FairPlay keybag context"))),
                Ok(status) if status_code(status) != 0 => errors.push(Some(Error::msg(format!(
                    "close FairPlay keybag context: status {}",
                    status_code(status)
                )))),
                Ok(_) => {}
            }
        }
        errors.push(machines.runtime.close().err());
        errors.push(machines.daemon.close().err());
        errors.push(machines.fairplay.close().err());
        if let Some(kernel) = machines.kernel.as_mut() {
            errors.push(kernel.close().err());
        }
        Error::join(errors).map_or(Ok(()), Err)
    }
}

impl Drop for ModernAdi {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl Machines {
    fn initialize_daemon(&mut self) -> Result<()> {
        let entry = self.daemon.invoke(ADID_ENTRY, &[0, 0, 0, 0]).err();
        let listener = self.daemon.state().xpc_event_handler;
        if listener == 0 {
            return Err(wrap_optional("initialize modern ADI daemon", entry));
        }
        let connection = self.daemon.state_mut().new_xpc_object(None);
        self.invoke_daemon_block(listener, connection)
            .context("accept modern ADI daemon connection")?;
        let handler = self.daemon.state().xpc_event_handler;
        if handler == listener {
            return Err(Error::msg(
                "modern ADI daemon did not install a request handler",
            ));
        }
        self.daemon_handler = handler;
        Ok(())
    }

    fn invoke_daemon_block(&mut self, block: u64, argument: u64) -> Result<()> {
        let handler = self.daemon.read_u64(block + 16)?;
        self.daemon.invoke(handler, &[block, argument]).map(drop)
    }

    /// Runs a client call twice: first to capture its daemon request, then with the daemon's
    /// reply available to `xpc_connection_send_message_with_reply_sync`.
    fn invoke(&mut self, function: u64, arguments: &[u64]) -> Result<u64> {
        let runtime = self.runtime.state_mut();
        runtime.xpc_data.clear();
        runtime.xpc_sync_reply = None;
        let first = self.runtime.invoke(function, arguments).err();
        let request = self.runtime.state().xpc_data.clone();
        if request.is_empty() {
            return Err(wrap_optional(
                "modern ADI client created no daemon request",
                first,
            ));
        }
        let daemon = self.daemon.state_mut();
        daemon.xpc_reply.clear();
        daemon.xpc_replied = false;
        let object = daemon.new_xpc_object(Some(&request));
        let handler = self.daemon_handler;
        self.invoke_daemon_block(handler, object)
            .context("run modern ADI daemon request")?;
        let daemon = self.daemon.state();
        if !daemon.xpc_replied || daemon.xpc_reply.is_empty() {
            return Err(Error::msg("modern ADI daemon returned no reply"));
        }
        let reply = daemon.xpc_reply.clone();
        self.runtime.state_mut().xpc_sync_reply = Some(reply);
        self.runtime.invoke(function, arguments)
    }

    fn read_output(&self, pointer_field: u64, length_field: u64) -> Result<Vec<u8>> {
        let pointer = self.runtime.read_u64(pointer_field)?;
        let length = self.runtime.read_u32(length_field)?;
        if pointer == 0 || length == 0 || length > 1 << 20 {
            return Err(Error::msg("modern ADI returned invalid output"));
        }
        self.runtime
            .uc
            .read_mem(pointer, u64::from(length))
            .context("read modern ADI output")
    }
}

fn initialize_fair_play_kernel(kernel: &mut Machine) -> Result<u64> {
    kernel.begin_call();
    let result = (|| {
        kernel
            .invoke(KEXT_START, &[])
            .context("start modern FairPlay kernel")?;
        let status = kernel
            .invoke(KEXT_SETUP, &[])
            .context("set up modern FairPlay kernel")?;
        let status = status_code(status);
        if status != 0 {
            return Err(Error::msg(format!(
                "set up modern FairPlay kernel: status {status}"
            )));
        }
        let output = kernel.scratch(&[], 8)?;
        let task = kernel
            .state_mut()
            .allocate(0x1000)
            .context("allocate modern FairPlay kernel task")?;
        kernel
            .uc
            .write_mem(task, &[0; 0x1000])
            .context("initialize modern FairPlay kernel task")?;
        let status = kernel
            .invoke(KEXT_INIT, &[0, output, 0, task])
            .context("initialize modern FairPlay kernel session")?;
        let status = status_code(status);
        if status != 0 {
            return Err(Error::msg(format!(
                "initialize modern FairPlay kernel session: status {status}"
            )));
        }
        let context = kernel.read_u64(output)?;
        if context == 0 {
            return Err(Error::msg(
                "modern FairPlay kernel returned no session context",
            ));
        }
        Ok(context)
    })();
    kernel.clear_scratch();
    result
}

fn truncated(input: &[u8], size: usize) -> Vec<u8> {
    input[..input.len().min(size)].to_vec()
}

fn handle_fair_play_message(
    route: &mut FairPlayRoute,
    input: &[u8],
    indirect: &[u8],
    output_size: usize,
) -> Result<(Vec<u8>, Vec<u8>, i32)> {
    if let Some(kernel) = route.kernel.as_mut() {
        let probe = kernel.read_u64(KEXT_BASE + 0x00b8_c028).unwrap_or_default();
        if probe != KEXT_BASE + 0x02c6_6248 {
            return Err(Error::msg(format!(
                "modern FairPlay kernel stack probe is {probe:#x}"
            )));
        }
        let context = route.kernel_context.get();
        kernel.begin_call();
        let result = (|| {
            let message = kernel.scratch(indirect, indirect.len() as u64)?;
            let status = kernel
                .invoke(KEXT_ENTRY, &[message, indirect.len() as u64, context])
                .context("run modern FairPlay kernel message")?;
            let updated = kernel
                .uc
                .read_mem(message, indirect.len() as u64)
                .context("read modern FairPlay kernel message")?;
            Ok((truncated(input, output_size), updated, status_code(status)))
        })();
        kernel.clear_scratch();
        return result;
    }
    let fairplay = &mut route.fairplay;
    let (callback, info) = {
        let state = fairplay.state();
        (state.mach_port_callback, state.mach_port_info)
    };
    if callback == 0 {
        return Err(Error::msg(
            "modern FairPlay daemon installed no message callback",
        ));
    }
    fairplay.begin_call();
    let result = (|| {
        let message = fairplay.scratch(indirect, indirect.len() as u64)?;
        fairplay
            .invoke(
                callback,
                &[FAKE_HANDLE, message, indirect.len() as u64, info],
            )
            .context("run modern FairPlay message callback")?;
        let updated = fairplay
            .uc
            .read_mem(message, indirect.len() as u64)
            .context("read modern FairPlay daemon message")?;
        Ok((truncated(input, output_size), updated, 0))
    })();
    fairplay.clear_scratch();
    result
}

fn handle_fair_play_mach_rpc(
    fairplay: &mut Machine,
    request: &[u8],
    receive_size: usize,
) -> Result<Vec<u8>> {
    let (callback, info) = {
        let state = fairplay.state();
        (state.mach_port_callback, state.mach_port_info)
    };
    if callback == 0 {
        return Err(Error::msg(
            "modern FairPlay daemon installed no message callback",
        ));
    }
    fairplay.begin_call();
    let result = (|| {
        let message = fairplay.scratch(request, request.len().max(receive_size) as u64)?;
        fairplay.state_mut().mach_message_reply.clear();
        fairplay
            .invoke(
                callback,
                &[FAKE_HANDLE, message, request.len() as u64, info],
            )
            .context("run modern FairPlay Mach callback")?;
        let reply = &fairplay.state().mach_message_reply;
        if reply.is_empty() {
            return Err(Error::msg("modern FairPlay daemon returned no Mach reply"));
        }
        Ok(reply.clone())
    })();
    fairplay.clear_scratch();
    result
}
