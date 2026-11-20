//! `CoreFoundation`, `IOKit`, XPC, Mach, libc and kernel services. Port of Go `shim_platform.go`.

use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::shim_memory::zeroes;
use super::shims::{
    Abi, Guest, Handler, MAX_GUEST_TRANSFER, REG_LR, REG_PC, REG_X0, Service, Uc, add_aliases,
    add_data, add_function, checked_size, handler, le64, low32,
};
use super::{HEAP_SIZE, PAGE_SIZE, ioaes};
use crate::error::{Context, Error, Result};

pub(crate) const FAKE_HANDLE: u64 = u64::MAX;
const CORE_FP_FILE: u64 = 3;
const ADI_FILE: u64 = 5;
const CORE_FP_PATH: &[u8] = b"/System/Library/PrivateFrameworks/CoreFP.framework/CoreFP";
const ICXS_PATH: &[u8] = b"./../CoreFP.icxs";
const KEY_SERIAL: &[u8] = b"IOPlatformSerialNumber";
const KEY_UUID: &[u8] = b"IOPlatformUUID";
const KEY_BOARD: &[u8] = b"board-id";
const KEYED_MESSAGE: &[u8] = b"objectForKey:";
const XPC_DICTIONARY_TYPE: u64 = 0xffff_1000_0000_0001;
pub(crate) const ADI_STATE_PATH: &str = "/var/db/fpsd/adi/adi.pb";

pub(crate) type RandomSource = std::rc::Rc<dyn Fn(&mut [u8]) -> Result<()>>;

/// Randomness and wall time seen by the guest.
#[derive(Clone)]
pub(crate) struct Host {
    pub(crate) random: RandomSource,
    pub(crate) now: std::rc::Rc<dyn Fn() -> SystemTime>,
}

#[cfg(test)]
thread_local! {
    /// The host every machine opened on this thread uses, for the Go oracle comparison.
    pub(crate) static TEST_HOST: std::cell::RefCell<Option<Host>> =
        const { std::cell::RefCell::new(None) };
}

impl Default for Host {
    fn default() -> Self {
        #[cfg(test)]
        if let Some(host) = TEST_HOST.with_borrow(Clone::clone) {
            return host;
        }
        Self {
            random: std::rc::Rc::new(|data| {
                getrandom::fill(data).map_err(|error| Error::msg(error.to_string()))
            }),
            now: std::rc::Rc::new(SystemTime::now),
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn register(uc: &mut Uc) -> Result<()> {
    let services: [(&[&str], Service); 83] = [
        (
            &[
                "_CFBundleGetMainBundle",
                "_CFDataGetBytePtr",
                "_CFDataGetLength",
                "_CFStringGetLength",
                "_CFStringGetMaximumSizeForEncoding",
                "_CFUUIDCreateString",
                "_IORegistryEntryFromPath",
                "_IORegistryEntrySearchCFProperty",
                "_getenv",
                "_pthread_self",
            ],
            return_zero,
        ),
        (
            &[
                "_CFDictionaryGetValue",
                "_CFFileDescriptorCreate",
                "_CFFileDescriptorCreateRunLoopSource",
                "_CFMachPortCreateRunLoopSource",
                "_CFRunLoopGetCurrent",
                "_CFRunLoopGetMain",
                "_CFURLCreateFromFileSystemRepresentation",
                "_DADiskCopyDescription",
                "_DADiskCreateFromBSDName",
                "_DASessionCreate",
                "_IORegistryEntryCreateCFProperty",
                "_dispatch_get_global_queue",
                "_dispatch_queue_create",
                "_dispatch_queue_create_with_target",
                "_os_transaction_create",
                "_xpc_connection_create",
            ],
            return_fake_handle,
        ),
        (
            &[
                "_CFRelease",
                "_CFFileDescriptorEnableCallBacks",
                "_CFFileDescriptorInvalidate",
                "_CFFileSecuritySetMode",
                "_CFRunLoopAddSource",
                "_CFURLCopyResourcePropertyForKey",
                "_CFURLSetResourcePropertyForKey",
                "_CSBackupIsItemExcluded",
                "_CSBackupSetItemExcluded",
                "_IOObjectRelease",
                "_IOLockLock",
                "_IOLockUnlock",
                "_IOLockWakeup",
                "_IORWLockRead",
                "_IORWLockWrite",
                "_IORWLockUnlock",
                "_lck_mtx_lock",
                "_lck_mtx_unlock",
                "_lck_rw_lock_exclusive",
                "_lck_rw_unlock_exclusive",
                "_chmod",
                "_close",
                "_close$UNIX2003",
                "_fchmod",
                "_fcntl",
                "_fcntl$UNIX2003",
                "_kevent",
                "_lockf",
                "_mkdir",
                "_pthread_mutex_lock",
                "_pthread_mutex_unlock",
                "_pthread_rwlock_init",
                "_pthread_rwlock_init$UNIX2003",
                "_pthread_rwlock_destroy",
                "_pthread_rwlock_rdlock",
                "_pthread_rwlock_unlock",
                "_pthread_rwlock_unlock$UNIX2003",
                "_pthread_rwlock_wrlock",
                "_pthread_rwlock_wrlock$UNIX2003",
                "_pthread_set_qos_class_self_np",
                "_CFRunLoopStop",
                "_getpid",
                "_getuid",
                "_notify_post",
                "_ptrace",
                "_sched_yield",
                "_signal",
                "_umask",
                "_os_release",
                "_xpc_connection_activate",
                "_xpc_connection_cancel",
                "_xpc_connection_resume",
                "_xpc_connection_set_target_queue",
                "_xpc_dictionary_set_bool",
                "_xpc_dictionary_set_int64",
                "_xpc_dictionary_set_string",
                "_xpc_dictionary_set_uint64",
                "_xpc_dictionary_set_value",
                "_xpc_release",
                "_xpc_retain",
                "_xpc_transaction_begin",
                "_xpc_transaction_end",
            ],
            return_zero,
        ),
        (&["_IOLog", "_IOLogv"], return_zero),
        (&["_notify_register_check"], notify_register_check),
        (&["_notify_check"], notify_check),
        (&["_notify_get_state"], notify_get_state),
        (&["_notify_cancel"], return_zero),
        (&["_atexit"], return_zero),
        (&["_CFRunLoopRun"], cf_run_loop_run),
        (
            &["_CFMachPortCreateWithPort"],
            cf_mach_port_create_with_port,
        ),
        (&["_CFMachPortGetPort"], return_one),
        (
            &[
                "_SecTaskCreateWithAuditToken",
                "_SecTaskCopyValueForEntitlement",
            ],
            return_fake_handle,
        ),
        (&["_CFStringCreateWithCString"], cf_string_create),
        (
            &["_CFStringCreateWithCStringNoCopy"],
            cf_string_create_no_copy,
        ),
        (&["_CFStringGetCString"], cf_string_get_c_string),
        (&["_CFGetTypeID", "_CFNumberGetTypeID"], return_one),
        (&["_CFStringGetTypeID"], return_two),
        (&["_CFDictionaryGetTypeID"], return_three),
        (&["_CFNumberGetValue"], cf_number_get_value),
        (&["___chkstk_darwin", "____chkstk_darwin"], return_unchanged),
        (
            &["__ZN9IOService15serviceMatchingEPKcP12OSDictionary"],
            return_unchanged,
        ),
        (
            &["__ZN9IOService14waitForServiceEP12OSDictionaryP13mach_timespec"],
            wait_for_kernel_service,
        ),
        (
            &["__ZN24IOBufferMemoryDescriptor17inTaskWithOptionsEP4taskjmm"],
            new_kernel_buffer_descriptor,
        ),
        (
            &["__ZN18IOMemoryDescriptor16withAddressRangeEyyjP4task"],
            new_kernel_buffer_descriptor_at,
        ),
        (&["_IOIteratorNext"], io_iterator_next),
        (
            &["_IOConnectCallStructMethod"],
            io_connect_call_struct_method,
        ),
        (
            &["_IORegistryEntryGetParentEntry"],
            io_registry_entry_get_parent_entry,
        ),
        (
            &["_IOServiceGetMatchingServices"],
            io_service_get_matching_services,
        ),
        (&["_IOServiceGetMatchingService"], return_one),
        (&["_IOServiceMatching"], return_fake_handle),
        (&["_IOMainPort"], io_main_port),
        (&["_IOServiceOpen"], io_service_open),
        (&["_OSAtomicCompareAndSwap32Barrier"], compare_and_swap32),
        (&["___error"], error_pointer),
        (&["_abort", "___stack_chk_fail", "dyld_stub_binder"], abort),
        (&["_arc4random"], arc4random),
        (&["_random"], random31),
        (&["_arc4random_uniform"], arc4random_uniform),
        (&["_read_random", "_kernel_read_random"], read_random),
        (&["_audit_token_to_au32"], audit_token_to_au32),
        (&["_audit_token_to_euid"], return_zero),
        (&["_basename"], basename),
        (&["_bootstrap_look_up"], bootstrap_look_up),
        (&["_bootstrap_check_in"], bootstrap_check_in),
        (
            &["_mach_msg_rpc_from_kernel_proper"],
            mach_msg_rpc_from_kernel,
        ),
        (&["_mach_msg", "_mach_msg2_internal"], mach_msg),
        (
            &[
                "_mach_msg_destroy",
                "_mach_msg_destroy_from_kernel_proper",
                "_mach_port_deallocate",
                "_mig_dealloc_reply_port",
                "_mig_deallocate",
                "_mig_put_reply_port",
            ],
            return_zero,
        ),
        (&["_mig_get_reply_port"], return_one),
        (&["_dispatch_once"], dispatch_once),
        (&["_dlopen"], dlopen),
        (&["_dlsym"], dlsym),
        (&["_statfs", "_statfs$INODE64"], return_minus_one),
        (&["_fstat"], fstat),
        (&["_ftruncate"], ftruncate),
        (&["_gettimeofday"], gettimeofday),
        (
            &["_clock_get_calendar_microtime"],
            clock_get_calendar_microtime,
        ),
        (&["_time"], current_time),
        (&["_gethostuuid"], gethostuuid),
        (&["_objc_msgSend"], objc_msg_send),
        (&["_open", "_open$UNIX2003"], open),
        (
            &["_lstat", "_lstat$INODE64", "_stat", "_stat$INODE64"],
            lstat,
        ),
        (&["_pthread_once"], pthread_once),
        (&["_proc_pidpath"], proc_pid_path),
        (&["_read", "_read$UNIX2003"], read),
        (&["_write"], write),
        (&["_unlink"], unlink),
        (&["_sysctl"], return_minus_one),
        (&["_sysctlbyname"], sysctlbyname),
        (&["_sysconf"], sysconf),
        (&["_xpc_dictionary_get_data"], xpc_dictionary_get_data),
        (&["_xpc_dictionary_set_data"], xpc_dictionary_set_data),
        (
            &["_xpc_connection_create_mach_service"],
            xpc_connection_create_mach_service,
        ),
        (
            &["_xpc_connection_send_message_with_reply_sync"],
            xpc_connection_send_message_with_reply_sync,
        ),
        (
            &["_xpc_connection_set_event_handler"],
            xpc_connection_set_event_handler,
        ),
        (
            &["_xpc_dictionary_create_empty"],
            xpc_dictionary_create_empty,
        ),
        (
            &["_xpc_dictionary_create_reply"],
            xpc_dictionary_create_reply,
        ),
        (
            &["_xpc_dictionary_get_audit_token"],
            xpc_dictionary_get_audit_token,
        ),
        (&["_xpc_dictionary_send_reply"], xpc_dictionary_send_reply),
        (&["_xpc_get_type"], xpc_get_type),
        (&["_kqueue"], kqueue),
        (&["_os_log_type_enabled"], return_one),
        (&["_os_log_impl", "__os_log_impl"], return_zero),
    ];
    for (names, function) in services {
        let service: Handler = handler(function);
        add_aliases(uc, names, &service)?;
    }
    let errno = add_data(uc, "guest.errno", &[0; 8])?;
    uc.get_data_mut().errno = errno;
    add_data(
        uc,
        "___stack_chk_guard",
        &[0xA5, 0x71, 0x3C, 0xD9, 0x86, 0x42, 0xEF, 0x10],
    )?;
    for name in [
        "_NDR_record",
        "_kCFAllocatorDefault",
        "_kCFAllocatorNull",
        "_kDADiskDescriptionVolumeUUIDKey",
        "_kIOMasterPortDefault",
    ] {
        add_data(uc, name, &[0; 8])?;
    }
    // Go ranges over a map here, so its order varies per run; this is Go's source order.
    for (name, value) in [
        ("_kCFBooleanFalse", FAKE_HANDLE),
        ("_kCFBooleanTrue", FAKE_HANDLE),
        ("__xpc_type_dictionary", XPC_DICTIONARY_TYPE),
        ("__xpc_type_error", XPC_DICTIONARY_TYPE + 1),
        ("__xpc_error_connection_invalid", XPC_DICTIONARY_TYPE + 2),
    ] {
        add_data(uc, name, &value.to_le_bytes())?;
    }
    add_function(uc, "callback.complete", handler(callback_complete))?;
    Ok(())
}

fn mach_msg_rpc_from_kernel(uc: &mut Uc) -> Result<()> {
    let message = uc.argument(0)?;
    let send_size = uc.argument(1)?;
    let receive_size = uc.argument(2)?;
    if send_size == 0 || send_size > 1 << 20 || receive_size > 1 << 20 {
        return Err(Error::msg("kernel Mach RPC size is invalid"));
    }
    let request = uc
        .read_mem(message, send_size)
        .context("read kernel Mach RPC request")?;
    let Some(rpc) = uc.get_data().mach_rpc_handler.clone() else {
        return Err(Error::msg("kernel Mach RPC handler is unavailable"));
    };
    let receive_size = checked_size(receive_size)?;
    let reply = rpc(&request, receive_size)?;
    if reply.is_empty() || reply.len() > receive_size {
        return Err(Error::msg("kernel Mach RPC reply is invalid"));
    }
    uc.write_mem(message, &reply)
        .context("write kernel Mach RPC reply")?;
    uc.set_result(0)
}

fn mach_msg(uc: &mut Uc) -> Result<()> {
    let message = uc.argument(0)?;
    let send_size = uc.argument(2)?;
    if send_size == 0 || send_size > 1 << 20 {
        return uc.set_result(0);
    }
    let reply = uc
        .read_mem(message, send_size)
        .context("read Mach message")?;
    uc.get_data_mut().mach_message_reply = reply;
    uc.set_result(0)
}

fn kqueue(uc: &mut Uc) -> Result<()> {
    uc.set_result(6)
}

fn cf_run_loop_run(uc: &mut Uc) -> Result<()> {
    if !uc.get_data().pause_run_loop {
        return return_zero(uc);
    }
    uc.stop().context("pause guest run loop")
}

fn cf_mach_port_create_with_port(uc: &mut Uc) -> Result<()> {
    let callback = uc.argument(2)?;
    let context = uc.argument(3)?;
    uc.get_data_mut().mach_port_callback = callback;
    uc.get_data_mut().mach_port_context = context;
    if context != 0 {
        let info = uc.read_u64(context + 8).unwrap_or_default();
        uc.get_data_mut().mach_port_info = info;
    }
    if let Ok(should_free_info) = uc.argument(4)
        && should_free_info != 0
    {
        uc.write_mem(should_free_info, &[0])
            .context("initialize Mach port release flag")?;
    }
    uc.set_result(FAKE_HANDLE)
}

fn xpc_connection_send_message_with_reply_sync(uc: &mut Uc) -> Result<()> {
    let state = uc.get_data_mut();
    let reply = state.xpc_sync_reply.clone();
    let object = state.new_xpc_object(reply.as_deref());
    uc.set_result(object)
}

fn new_empty_xpc_object(uc: &mut Uc) -> Result<()> {
    let object = uc.get_data_mut().new_xpc_object(None);
    uc.set_result(object)
}

fn xpc_connection_create_mach_service(uc: &mut Uc) -> Result<()> {
    new_empty_xpc_object(uc)
}

fn xpc_connection_set_event_handler(uc: &mut Uc) -> Result<()> {
    let block = uc.argument(1)?;
    let copy = uc.get_data_mut().allocate(64)?;
    let data = uc.read_mem(block, 64).context("copy XPC event handler")?;
    uc.write_mem(copy, &data)
        .context("store XPC event handler")?;
    uc.get_data_mut().xpc_event_handler = copy;
    uc.set_result(0)
}

fn xpc_dictionary_create_empty(uc: &mut Uc) -> Result<()> {
    new_empty_xpc_object(uc)
}

fn xpc_dictionary_create_reply(uc: &mut Uc) -> Result<()> {
    new_empty_xpc_object(uc)
}

fn xpc_dictionary_get_audit_token(uc: &mut Uc) -> Result<()> {
    let token = uc.argument(1)?;
    if token != 0 {
        let mut value = [0u8; 32];
        value[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        value[20..24].copy_from_slice(&123u32.to_le_bytes());
        value[24..28].copy_from_slice(&100_000u32.to_le_bytes());
        value[28..32].copy_from_slice(&1u32.to_le_bytes());
        uc.write_mem(token, &value)
            .context("write XPC audit token")?;
    }
    uc.set_result(0)
}

fn xpc_dictionary_send_reply(uc: &mut Uc) -> Result<()> {
    let object = uc.argument(0)?;
    let state = uc.get_data_mut();
    if let Some(value) = state.xpc_objects.get(&object) {
        state.xpc_reply = value.data.clone();
    }
    state.xpc_replied = true;
    uc.set_result(0)
}

fn xpc_get_type(uc: &mut Uc) -> Result<()> {
    let object = uc.argument(0)?;
    let state = uc.get_data();
    if state.xpc_objects.contains_key(&object) {
        let dictionary = state
            .symbols
            .get("__xpc_type_dictionary")
            .copied()
            .unwrap_or_default();
        return uc.set_result(dictionary);
    }
    uc.set_result(0)
}

fn xpc_dictionary_set_data(uc: &mut Uc) -> Result<()> {
    let object = uc.argument(0)?;
    let address = uc.argument(2)?;
    let length = uc.argument(3)?;
    if length > 1 << 20 {
        return Err(Error::msg("XPC data exceeds guest limit"));
    }
    let data = uc.read_mem(address, length).context("read XPC data")?;
    let state = uc.get_data_mut();
    if let Some(value) = state.xpc_objects.get_mut(&object) {
        value.data.clone_from(&data);
        value.has_data = true;
    }
    state.xpc_data = data;
    uc.set_result(0)
}

fn xpc_dictionary_get_data(uc: &mut Uc) -> Result<()> {
    let object = uc.argument(0)?;
    let length = uc.argument(2)?;
    let (has_data, data_address, size) = match uc.get_data().xpc_objects.get(&object) {
        Some(value) if value.has_data => (true, value.data_address, value.data.len()),
        _ => (false, 0, 0),
    };
    if !has_data {
        if length != 0 {
            uc.write_u64(length, 0)?;
        }
        return uc.set_result(0);
    }
    let mut data_address = data_address;
    if data_address == 0 {
        data_address = uc.get_data_mut().allocate(size.max(1) as u64)?;
        let data = uc
            .get_data_mut()
            .xpc_objects
            .get_mut(&object)
            .map_or_else(Vec::new, |value| {
                value.data_address = data_address;
                value.data.clone()
            });
        uc.write_mem(data_address, &data)
            .context("write XPC data")?;
    }
    if length != 0 {
        uc.write_u64(length, size as u64)?;
    }
    uc.set_result(data_address)
}

pub(crate) fn return_zero(uc: &mut Uc) -> Result<()> {
    uc.set_result(0)
}

fn return_one(uc: &mut Uc) -> Result<()> {
    uc.set_result(1)
}

fn return_two(uc: &mut Uc) -> Result<()> {
    uc.set_result(2)
}

fn return_three(uc: &mut Uc) -> Result<()> {
    uc.set_result(3)
}

#[allow(clippy::unnecessary_wraps, reason = "guest service signature")]
fn return_unchanged(_: &mut Uc) -> Result<()> {
    Ok(())
}

fn return_fake_handle(uc: &mut Uc) -> Result<()> {
    uc.set_result(FAKE_HANDLE)
}

pub(crate) fn return_minus_one(uc: &mut Uc) -> Result<()> {
    uc.set_result(u64::MAX)
}

fn cf_string_create(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(1)?;
    let value = uc.read_c_string(address)?;
    if [KEY_SERIAL, KEY_UUID, KEY_BOARD].contains(&value.as_slice()) {
        return uc.set_result(FAKE_HANDLE);
    }
    uc.set_result(0)
}

fn cf_number_get_value(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(2)?;
    if address != 0 {
        uc.write_u32(address, 3_184_554_750)?;
    }
    uc.set_result(1)
}

fn cf_string_create_no_copy(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(1)?;
    uc.set_result(address)
}

fn cf_string_get_c_string(uc: &mut Uc) -> Result<()> {
    let value_address = uc.argument(0)?;
    let buffer = uc.argument(1)?;
    let capacity = uc.argument(2)?;
    if buffer == 0 || capacity == 0 {
        return uc.set_result(0);
    }
    let mut encoded = Vec::new();
    if value_address != 0
        && value_address != FAKE_HANDLE
        && let Ok(decoded) = uc.read_c_string(value_address)
    {
        encoded = decoded;
    }
    encoded.push(0);
    if encoded.len() as u64 > capacity {
        return uc.set_result(0);
    }
    uc.write_mem(buffer, &encoded)
        .context("write guest string buffer")?;
    uc.set_result(1)
}

fn io_iterator_next(uc: &mut Uc) -> Result<()> {
    let state = uc.get_data_mut();
    state.iterator = state.iterator.wrapping_add(1);
    let value = u64::from(state.iterator % 2);
    uc.set_result(value)
}

fn io_connect_call_struct_method(uc: &mut Uc) -> Result<()> {
    let selector = uc.argument(1)?;
    let input_size = uc.argument(3)?;
    let output_size_address = uc.argument(5)?;
    let mut output_size = 0;
    if output_size_address != 0 {
        output_size = uc.read_u64(output_size_address).unwrap_or_default();
    }
    if input_size > 1 << 20 || output_size > 1 << 20 {
        return Err(Error::msg("FairPlay IOKit buffer exceeds guest limit"));
    }
    let input_address = uc.argument(2)?;
    let input = uc
        .read_mem(input_address, input_size)
        .context("read FairPlay IOKit input")?;
    let mut indirect = Vec::new();
    let mut indirect_address = 0;
    if selector == 26 && input.len() == 16 {
        indirect_address = le64(&input[..8]);
        let indirect_size = le64(&input[8..]);
        if indirect_size == 0 || indirect_size > 1 << 20 {
            return Err(Error::msg("FairPlay IOKit indirect buffer is invalid"));
        }
        indirect = uc
            .read_mem(indirect_address, indirect_size)
            .context("read FairPlay IOKit indirect buffer")?;
    }
    let Some(fair_play) = uc.get_data().fair_play_handler.clone() else {
        return Err(Error::msg("emulated FairPlay IOKit handler is unavailable"));
    };
    let output_size = checked_size(output_size)?;
    let (mut output, mut indirect_output, mut code) = fair_play(&input, &indirect, output_size)?;
    if code != 0 && selector == 26 && indirect.len() >= 16 && input.len() == output_size {
        indirect_output.clone_from(&indirect);
        let split = indirect_output.len() - 16;
        let digest = Sha256::digest(&indirect_output[..split]);
        indirect_output[split..].copy_from_slice(&digest[..16]);
        output.clone_from(&input);
        code = 0;
    }
    if !indirect_output.is_empty() {
        uc.write_mem(indirect_address, &indirect_output)
            .context("write FairPlay IOKit indirect buffer")?;
        if output.len() >= 8 {
            output[..8].copy_from_slice(&indirect_address.to_le_bytes());
        }
    }
    let output_address = uc.argument(4)?;
    uc.write_mem(output_address, &output)
        .context("write FairPlay IOKit output")?;
    if output_size_address != 0 {
        uc.write_u64(output_size_address, output.len() as u64)?;
    }
    uc.set_result(u64::from(code.cast_unsigned()))
}

fn io_main_port(uc: &mut Uc) -> Result<()> {
    let port = uc.argument(1)?;
    if port != 0 {
        uc.write_u32(port, 1)?;
    }
    uc.set_result(0)
}

fn fill_pointers(table: &mut [u8], value: u64) {
    for slot in table.as_chunks_mut::<8>().0 {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

fn put_pointer(table: &mut [u8], offset: usize, value: u64) {
    table[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn wait_for_kernel_service(uc: &mut Uc) -> Result<()> {
    let name_address = uc.argument(0)?;
    let name = uc.read_c_string(name_address)?;
    if name != b"IOAESAccelerator" {
        return uc.set_result(0);
    }
    let service = uc.get_data().aes_service;
    if service != 0 {
        return uc.set_result(service);
    }
    let unhandled = add_function(uc, "$IOAESAccelerator.unhandled", handler(return_zero))?;
    let retain = add_function(uc, "$IOAESAccelerator.retain", handler(return_unchanged))?;
    let perform = add_function(uc, "$IOAESAccelerator.performAES", handler(io_aes_perform))?;
    let mut vtable = vec![0u8; 0x900];
    fill_pointers(&mut vtable, unhandled);
    put_pointer(&mut vtable, 0x20, retain);
    put_pointer(&mut vtable, 0x550, perform);
    put_pointer(&mut vtable, 0x890, perform);
    let vtable = add_data(uc, "kernel.ioaes.vtable", &vtable)?;
    uc.get_data_mut().aes_vtable = vtable;
    let service = uc.get_data_mut().allocate(16)?;
    uc.get_data_mut().aes_service = service;
    uc.write_u64(service, vtable)?;
    uc.set_result(service)
}

fn new_kernel_buffer_descriptor(uc: &mut Uc) -> Result<()> {
    let capacity = uc.argument(2)?;
    if capacity == 0 || capacity > MAX_GUEST_TRANSFER {
        return uc.set_result(0);
    }
    let buffer = uc.get_data_mut().allocate_aligned(capacity, 16)?;
    uc.write_mem(buffer, &zeroes(capacity)?)
        .context("initialize kernel buffer")?;
    create_kernel_buffer_descriptor(uc, buffer, capacity)
}

fn new_kernel_buffer_descriptor_at(uc: &mut Uc) -> Result<()> {
    let buffer = uc.argument(0)?;
    let capacity = uc.argument(1)?;
    if buffer == 0 || capacity == 0 || capacity > MAX_GUEST_TRANSFER {
        return uc.set_result(0);
    }
    create_kernel_buffer_descriptor(uc, buffer, capacity)
}

fn create_kernel_buffer_descriptor(uc: &mut Uc, buffer: u64, capacity: u64) -> Result<()> {
    if uc.get_data().buffer_vtable == 0 {
        let unhandled = add_function(
            uc,
            "$IOBufferMemoryDescriptor.unhandled",
            handler(unsupported_kernel_buffer_method),
        )?;
        let prepare = add_function(
            uc,
            "$IOBufferMemoryDescriptor.prepare",
            handler(kernel_buffer_prepare),
        )?;
        let complete = add_function(
            uc,
            "$IOBufferMemoryDescriptor.complete",
            handler(return_zero),
        )?;
        let address = add_function(
            uc,
            "$IOBufferMemoryDescriptor.getVirtualAddress",
            handler(kernel_buffer_address),
        )?;
        let read = add_function(
            uc,
            "$IOMemoryDescriptor.readBytes",
            handler(kernel_buffer_read),
        )?;
        let write = add_function(
            uc,
            "$IOMemoryDescriptor.writeBytes",
            handler(kernel_buffer_write),
        )?;
        let release = add_function(
            uc,
            "$IOBufferMemoryDescriptor.release",
            handler(return_zero),
        )?;
        let mut vtable = vec![0u8; 0x1000];
        fill_pointers(&mut vtable, unhandled);
        put_pointer(&mut vtable, 0x28, release);
        put_pointer(&mut vtable, 0xc8, read);
        put_pointer(&mut vtable, 0xd0, write);
        put_pointer(&mut vtable, 0xd8, prepare);
        put_pointer(&mut vtable, 0xe0, complete);
        put_pointer(&mut vtable, 0x148, address);
        put_pointer(&mut vtable, 0x218, prepare);
        put_pointer(&mut vtable, 0x220, complete);
        put_pointer(&mut vtable, 0x308, address);
        let vtable = add_data(uc, "kernel.iobuffer.vtable", &vtable)?;
        uc.get_data_mut().buffer_vtable = vtable;
    }
    let descriptor = uc.get_data_mut().allocate(24)?;
    let vtable = uc.get_data().buffer_vtable;
    uc.write_u64(descriptor, vtable)?;
    uc.write_u64(descriptor + 8, buffer)?;
    uc.write_u64(descriptor + 16, capacity)?;
    uc.set_result(descriptor)
}

fn unsupported_kernel_buffer_method(uc: &mut Uc) -> Result<()> {
    let caller = uc.reg(REG_LR).unwrap_or_default();
    Err(Error::msg(format!(
        "unsupported IOMemoryDescriptor virtual method from {caller:#x}"
    )))
}

fn kernel_buffer_prepare(uc: &mut Uc) -> Result<()> {
    let descriptor = uc.argument(0)?;
    let buffer = uc.read_u64(descriptor + 8)?;
    let capacity = uc.read_u64(descriptor + 16)?;
    if buffer == 0 || capacity == 0 || capacity > MAX_GUEST_TRANSFER {
        return uc.set_result(3);
    }
    uc.set_result(0)
}

fn kernel_buffer_address(uc: &mut Uc) -> Result<()> {
    let descriptor = uc.argument(0)?;
    let address = uc.read_u64(descriptor + 8)?;
    uc.set_result(address)
}

fn kernel_buffer_read(uc: &mut Uc) -> Result<()> {
    transfer_kernel_buffer(uc, false)
}

fn kernel_buffer_write(uc: &mut Uc) -> Result<()> {
    transfer_kernel_buffer(uc, true)
}

fn transfer_kernel_buffer(uc: &mut Uc, write: bool) -> Result<()> {
    let descriptor = uc.argument(0)?;
    let offset = uc.argument(1)?;
    let address = uc.argument(2)?;
    let length = uc.argument(3)?;
    let buffer = uc.read_u64(descriptor + 8)?;
    let capacity = uc.read_u64(descriptor + 16)?;
    if offset > capacity || length > capacity - offset || length > MAX_GUEST_TRANSFER {
        return uc.set_result(0);
    }
    let (source, destination) = if write {
        (address, buffer + offset)
    } else {
        (buffer + offset, address)
    };
    let data = uc
        .read_mem(source, length)
        .context("read kernel buffer transfer")?;
    uc.write_mem(destination, &data)
        .context("write kernel buffer transfer")?;
    uc.set_result(length)
}

fn io_aes_perform(uc: &mut Uc) -> Result<()> {
    let mut arguments = [0u64; 8];
    for (index, argument) in arguments.iter_mut().enumerate() {
        *argument = uc.argument(index)?;
    }
    let length = arguments[3];
    if length == 0 || length > MAX_GUEST_TRANSFER || length % 16 != 0 {
        return uc.set_result(3);
    }
    let source_base = uc.read_u64(arguments[1] + 8)?;
    let source_capacity = uc.read_u64(arguments[1] + 16)?;
    let destination_base = uc.read_u64(arguments[2] + 8)?;
    let destination_capacity = uc.read_u64(arguments[2] + 16)?;
    let destination_offset = uc.argument(8)?;
    if arguments[7] > source_capacity
        || length > source_capacity - arguments[7]
        || destination_offset > destination_capacity
        || length > destination_capacity - destination_offset
    {
        return uc.set_result(3);
    }
    let source = uc
        .read_mem(source_base + arguments[7], length)
        .context("read IOAES input")?;
    let iv = uc
        .read_mem(arguments[4], 16)
        .context("read IOAES initialization vector")?;
    let key = uc
        .read_mem(arguments[6], 40)
        .context("read IOAES key data")?;
    let (destination, output_iv) = ioaes::emulate_io_aes(&source, &iv, low32(arguments[5]), &key)?;
    if destination.len() != source.len() || output_iv.len() != 16 {
        return Err(Error::msg("emulated IOAES returned invalid output"));
    }
    uc.write_mem(destination_base + destination_offset, &destination)
        .context("write IOAES output")?;
    uc.write_mem(arguments[4], &output_iv)
        .context("write IOAES initialization vector")?;
    uc.set_result(0)
}

fn io_service_open(uc: &mut Uc) -> Result<()> {
    let connection = uc.argument(3)?;
    if connection != 0 {
        uc.write_u32(connection, 7)?;
    }
    uc.set_result(0)
}

fn io_registry_entry_get_parent_entry(uc: &mut Uc) -> Result<()> {
    let parent = uc.argument(2)?;
    if parent == 0 {
        return Err(Error::msg("parent registry entry output is null"));
    }
    uc.write_u32(parent, u32::MAX)?;
    uc.set_result(0)
}

fn io_service_get_matching_services(uc: &mut Uc) -> Result<()> {
    let iterator = uc.argument(2)?;
    if iterator == 0 {
        return Err(Error::msg("matching services iterator output is null"));
    }
    uc.get_data_mut().iterator = 0;
    uc.write_u32(iterator, u32::MAX)?;
    uc.set_result(0)
}

fn compare_and_swap32(uc: &mut Uc) -> Result<()> {
    let old_value = uc.argument(0)?;
    let new_value = uc.argument(1)?;
    let address = uc.argument(2)?;
    let current = uc.read_u32(address)?;
    if current != low32(old_value) {
        return uc.set_result(0);
    }
    uc.write_u32(address, low32(new_value))?;
    uc.set_result(1)
}

fn error_pointer(uc: &mut Uc) -> Result<()> {
    let errno = uc.get_data().errno;
    uc.set_result(errno)
}

fn abort(_: &mut Uc) -> Result<()> {
    Err(Error::msg("guest aborted"))
}

fn random_u32(uc: &Uc, context: &str) -> Result<u32> {
    let mut value = [0u8; 4];
    (uc.get_data().host.random)(&mut value).context(context.to_owned())?;
    Ok(u32::from_le_bytes(value))
}

fn arc4random(uc: &mut Uc) -> Result<()> {
    let value = random_u32(uc, "generate guest random value")?;
    uc.set_result(u64::from(value))
}

fn random31(uc: &mut Uc) -> Result<()> {
    let value = random_u32(uc, "generate guest random value")?;
    uc.set_result(u64::from(value & 0x7fff_ffff))
}

fn arc4random_uniform(uc: &mut Uc) -> Result<()> {
    let upper = uc.argument(0)?;
    if upper < 2 {
        return uc.set_result(0);
    }
    let mut limit = u64::from(u32::MAX) + 1;
    limit -= limit % upper;
    loop {
        let value = u64::from(random_u32(uc, "generate bounded guest random value")?);
        if value < limit {
            return uc.set_result(value % upper);
        }
    }
}

fn read_random(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(0)?;
    let length = uc.argument(1)?;
    if length > MAX_GUEST_TRANSFER {
        return Err(Error::msg(format!(
            "random request size {length} exceeds limit"
        )));
    }
    let mut data = zeroes(length)?;
    (uc.get_data().host.random)(&mut data).context("generate guest random bytes")?;
    uc.write_mem(address, &data)
        .context("write guest random bytes")?;
    uc.set_result(0)
}

fn notify_register_check(uc: &mut Uc) -> Result<()> {
    let token = uc.argument(1)?;
    if token != 0 {
        uc.write_u32(token, 1)?;
    }
    uc.set_result(0)
}

fn notify_check(uc: &mut Uc) -> Result<()> {
    let changed = uc.argument(1)?;
    if changed != 0 {
        uc.write_u32(changed, 0)?;
    }
    uc.set_result(0)
}

fn notify_get_state(uc: &mut Uc) -> Result<()> {
    let state = uc.argument(1)?;
    if state != 0 {
        uc.write_u64(state, 0)?;
    }
    uc.set_result(0)
}

fn audit_token_to_au32(uc: &mut Uc) -> Result<()> {
    for (index, value) in [0u32, 0, 0, 0, 0, 123, 100_000, 1].into_iter().enumerate() {
        let address = uc.argument(index + 1)?;
        if address != 0 {
            uc.write_u32(address, value)?;
        }
    }
    uc.set_result(0)
}

fn basename(uc: &mut Uc) -> Result<()> {
    let mut address = uc.argument(0)?;
    let value = uc.read_c_string(address)?;
    let trimmed = value.len() - value.iter().rev().take_while(|&&byte| byte == b'/').count();
    if let Some(index) = value[..trimmed].iter().rposition(|&byte| byte == b'/') {
        address += index as u64 + 1;
    }
    uc.set_result(address)
}

fn lstat(uc: &mut Uc) -> Result<()> {
    let path_address = uc.argument(0)?;
    let path = uc.read_c_string(path_address)?;
    let is_directory =
        [&b"/var/db/fpsd/adi"[..], b"/var/db", b"/var/setup"].contains(&path.as_slice());
    let is_file =
        std::str::from_utf8(&path).is_ok_and(|path| uc.get_data().adi_files.contains_key(path));
    if !(is_directory || (is_adi_state_path(&path) && is_file)) {
        return return_minus_one(uc);
    }
    let result = uc.argument(1)?;
    let mode: [u8; 2] = if is_file { [0x80, 0x81] } else { [0xed, 0x41] };
    // Darwin's stat structure stores st_mode at byte offset four.
    uc.write_mem(result + 4, &mode)
        .context("write guest stat mode")?;
    uc.set_result(0)
}

fn bootstrap_look_up(uc: &mut Uc) -> Result<()> {
    let service = uc.argument(2)?;
    if service != 0 {
        uc.write_u32(service, 0)?;
    }
    uc.set_result(5) // KERN_FAILURE
}

fn bootstrap_check_in(uc: &mut Uc) -> Result<()> {
    let port = uc.argument(2)?;
    if port != 0 {
        uc.write_u32(port, 1)?;
    }
    uc.set_result(0)
}

fn dlopen(uc: &mut Uc) -> Result<()> {
    let path_address = uc.argument(0)?;
    let path = uc.read_c_string(path_address)?;
    if path == CORE_FP_PATH {
        return uc.set_result(FAKE_HANDLE);
    }
    uc.set_result(0)
}

fn dlsym(uc: &mut Uc) -> Result<()> {
    let name_address = uc.argument(1)?;
    let name = uc.read_c_string(name_address)?;
    let mut symbol = b"_".to_vec();
    symbol.extend_from_slice(&name);
    let address = std::str::from_utf8(&symbol)
        .ok()
        .and_then(|symbol| uc.get_data().core_exports.get(symbol).copied())
        .unwrap_or_default();
    uc.set_result(address)
}

fn unix_now(uc: &Uc) -> (u64, u32) {
    let now = (uc.get_data().host.now)();
    let elapsed = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    (elapsed.as_secs(), elapsed.subsec_micros())
}

fn current_time(uc: &mut Uc) -> Result<()> {
    let address = uc.argument(0)?;
    let (seconds, _) = unix_now(uc);
    if address != 0 {
        uc.write_u64(address, seconds)?;
    }
    uc.set_result(seconds)
}

fn clock_get_calendar_microtime(uc: &mut Uc) -> Result<()> {
    let seconds_address = uc.argument(0)?;
    let microseconds_address = uc.argument(1)?;
    let (seconds, microseconds) = unix_now(uc);
    uc.write_u32(seconds_address, low32(seconds))?;
    uc.write_u32(microseconds_address, microseconds)?;
    uc.set_result(0)
}

fn gettimeofday(uc: &mut Uc) -> Result<()> {
    let time_address = uc.argument(0)?;
    let zone_address = uc.argument(1)?;
    let (seconds, microseconds) = unix_now(uc);
    if time_address != 0 {
        let mut value = [0u8; 16];
        value[0..8].copy_from_slice(&seconds.to_le_bytes());
        value[8..12].copy_from_slice(&microseconds.to_le_bytes());
        uc.write_mem(time_address, &value)
            .context("write guest time value")?;
    }
    if zone_address != 0 {
        uc.write_mem(zone_address, &[0; 8])
            .context("clear guest time zone")?;
    }
    uc.set_result(0)
}

fn gethostuuid(uc: &mut Uc) -> Result<()> {
    let uuid = uc.get_data().host_uuid;
    if uuid == [0; 16] {
        return Err(Error::msg("guest host UUID is unavailable"));
    }
    let address = uc.argument(0)?;
    if address != 0 {
        uc.write_mem(address, &uuid)
            .context("write guest host UUID")?;
    }
    uc.set_result(0)
}

fn objc_msg_send(uc: &mut Uc) -> Result<()> {
    let selector_address = uc.argument(1)?;
    let selector = uc.read_c_string(selector_address)?;
    if selector == KEYED_MESSAGE {
        return uc.set_result(FAKE_HANDLE);
    }
    uc.set_result(0)
}

fn open(uc: &mut Uc) -> Result<()> {
    let path_address = uc.argument(0)?;
    let path = uc.read_c_string(path_address)?;
    let flags = uc.argument(1)?;
    if path == b"/var/db" || path == b"/var/setup" {
        return uc.set_result(4);
    }
    if is_adi_state_path(&path) {
        let state = uc.get_data_mut();
        if !state.adi_files.contains_key(ADI_STATE_PATH) {
            if flags & 0x200 == 0 {
                // O_CREAT
                return return_minus_one(uc);
            }
            state
                .adi_files
                .insert(ADI_STATE_PATH.to_owned(), Vec::new());
        }
        ADI_STATE_PATH.clone_into(&mut state.adi_path);
        state.adi_offset = 0;
        return uc.set_result(ADI_FILE);
    }
    if path == ICXS_PATH {
        uc.get_data_mut().icxs_offset = 0;
        return uc.set_result(CORE_FP_FILE);
    }
    return_minus_one(uc)
}

fn current_adi_file<'a>(uc: &'a mut Uc<'_>) -> &'a mut Vec<u8> {
    let state = uc.get_data_mut();
    state.adi_files.entry(state.adi_path.clone()).or_default()
}

fn adi_file_length(uc: &Uc) -> usize {
    let state = uc.get_data();
    state.adi_files.get(&state.adi_path).map_or(0, Vec::len)
}

fn fstat(uc: &mut Uc) -> Result<()> {
    let descriptor = uc.argument(0)?;
    if descriptor != ADI_FILE {
        return return_minus_one(uc);
    }
    let result = uc.argument(1)?;
    uc.write_mem(result, &[0; 144])
        .context("clear guest stat")?;
    uc.write_mem(result + 4, &[0x80, 0x81])
        .context("write guest file mode")?;
    let length = adi_file_length(uc) as u64;
    uc.write_u64(result + 96, length)?;
    uc.set_result(0)
}

fn ftruncate(uc: &mut Uc) -> Result<()> {
    let descriptor = uc.argument(0)?;
    if descriptor != ADI_FILE {
        return return_minus_one(uc);
    }
    let length = uc.argument(1)?;
    if length > 1 << 20 {
        return return_minus_one(uc);
    }
    let length = checked_size(length)?;
    let data = current_adi_file(uc);
    data.resize(length, 0);
    let state = uc.get_data_mut();
    state.adi_offset = state.adi_offset.min(length);
    uc.set_result(0)
}

fn pthread_once(uc: &mut Uc) -> Result<()> {
    let control = uc.argument(0)?;
    let initializer = uc.argument(1)?;
    let value = uc.read_u64(control)?;
    if value == 0 || value == 0x4f4e_4345 {
        return uc.set_result(0);
    }
    uc.write_u64(control, 0x4f4e_4345)?;
    invoke_callback(uc, initializer)
}

fn proc_pid_path(uc: &mut Uc) -> Result<()> {
    let buffer = uc.argument(1)?;
    let size = uc.argument(2)?;
    let path: &[u8] =
        b"/System/Library/PrivateFrameworks/CommerceKit.framework/Versions/A/Resources/commerce\0";
    if buffer == 0 || size < path.len() as u64 {
        return return_minus_one(uc);
    }
    uc.write_mem(buffer, path)
        .context("write guest process path")?;
    uc.set_result(path.len() as u64 - 1)
}

fn dispatch_once(uc: &mut Uc) -> Result<()> {
    let predicate = uc.argument(0)?;
    let block = uc.argument(1)?;
    let value = uc.read_u64(predicate)?;
    if value == u64::MAX {
        return uc.set_result(0);
    }
    uc.write_u64(predicate, u64::MAX)?;
    let initializer = uc.read_u64(block + 16)?;
    uc.set_reg(REG_X0, block)
        .context("write dispatch block argument")?;
    invoke_callback(uc, initializer)
}

fn invoke_callback(uc: &mut Uc, initializer: u64) -> Result<()> {
    let return_address = uc.reg(REG_LR).context("read guest return register")?;
    let state = uc.get_data_mut();
    state.callback_returns.push(return_address);
    let completion = state
        .symbols
        .get("callback.complete")
        .copied()
        .unwrap_or_default();
    uc.set_reg(REG_LR, completion)
        .context("write callback return register")?;
    uc.set_reg(REG_PC, initializer)
        .context("write callback instruction register")
}

fn callback_complete(uc: &mut Uc) -> Result<()> {
    let Some(return_address) = uc.get_data_mut().callback_returns.pop() else {
        return Err(Error::msg(
            "guest callback completion has no return address",
        ));
    };
    uc.set_reg(REG_LR, return_address)
        .context("restore guest return register")?;
    uc.set_result(0)
}

fn read(uc: &mut Uc) -> Result<()> {
    let descriptor = uc.argument(0)?;
    let buffer = uc.argument(1)?;
    let requested = uc.argument(2)?;
    if descriptor == ADI_FILE {
        let size = checked_size(requested)?;
        let state = uc.get_data();
        let data = state
            .adi_files
            .get(&state.adi_path)
            .cloned()
            .unwrap_or_default();
        let offset = state.adi_offset;
        let size = size.min(data.len().saturating_sub(offset));
        if size != 0 {
            uc.write_mem(buffer, &data[offset..offset + size])
                .context("write ADI state to guest")?;
            uc.get_data_mut().adi_offset += size;
        }
        return uc.set_result(size as u64);
    }
    if descriptor != CORE_FP_FILE {
        return return_minus_one(uc);
    }
    let size = checked_size(requested)?;
    let state = uc.get_data();
    let icxs = state.icxs.clone();
    let offset = state.icxs_offset;
    let size = size.min(icxs.len().saturating_sub(offset));
    if size != 0 {
        uc.write_mem(buffer, &icxs[offset..offset + size])
            .context("write guest ICXS data")?;
        uc.get_data_mut().icxs_offset += size;
    }
    uc.set_result(size as u64)
}

fn write(uc: &mut Uc) -> Result<()> {
    let descriptor = uc.argument(0)?;
    if descriptor != ADI_FILE {
        return return_minus_one(uc);
    }
    let buffer = uc.argument(1)?;
    let requested = uc.argument(2)?;
    let size = checked_size(requested)?;
    let data = uc
        .read_mem(buffer, size as u64)
        .context("read ADI state from guest")?;
    let offset = uc.get_data().adi_offset;
    let end = offset + size;
    let state = current_adi_file(uc);
    if end > state.len() {
        state.resize(end, 0);
    }
    state[offset..end].copy_from_slice(&data);
    uc.get_data_mut().adi_offset = end;
    uc.set_result(size as u64)
}

fn unlink(uc: &mut Uc) -> Result<()> {
    let path_address = uc.argument(0)?;
    let path = uc.read_c_string(path_address)?;
    if is_adi_state_path(&path) {
        let state = uc.get_data_mut();
        state.adi_files.remove(ADI_STATE_PATH);
        state.adi_offset = 0;
        return uc.set_result(0);
    }
    return_minus_one(uc)
}

fn is_adi_state_path(path: &[u8]) -> bool {
    path == ADI_STATE_PATH.as_bytes()
}

fn sysctlbyname(uc: &mut Uc) -> Result<()> {
    let name_address = uc.argument(0)?;
    let name = uc.read_c_string(name_address)?;
    let output = uc.argument(1)?;
    let length_address = uc.argument(2)?;
    let value: Vec<u8> = match name.as_slice() {
        b"kern.monotonicclock"
        | b"hw.ncpu"
        | b"hw.logicalcpu"
        | b"hw.physicalcpu"
        | b"hw.optional.arm64" => 1u32.to_le_bytes().to_vec(),
        b"hw.memsize" => (8u64 << 30).to_le_bytes().to_vec(),
        b"hw.machine" => b"arm64\0".to_vec(),
        b"hw.model" => b"MacBookPro13,3\0".to_vec(),
        b"kern.osversion" => b"16G29\0".to_vec(),
        _ => return return_minus_one(uc),
    };
    if length_address != 0 {
        let requested = uc.read_u64(length_address)?;
        if output != 0 && requested < value.len() as u64 {
            return return_minus_one(uc);
        }
        if output != 0 {
            uc.write_mem(output, &value)
                .context("write sysctl output")?;
        }
        uc.write_u64(length_address, value.len() as u64)?;
    }
    uc.set_result(0)
}

fn sysconf(uc: &mut Uc) -> Result<()> {
    match uc.argument(0)? {
        29 => uc.set_result(PAGE_SIZE),              // _SC_PAGESIZE
        57 | 58 => uc.set_result(1),                 // _SC_NPROCESSORS_CONF, _SC_NPROCESSORS_ONLN
        200 => uc.set_result(HEAP_SIZE / PAGE_SIZE), // _SC_PHYS_PAGES
        _ => return_minus_one(uc),
    }
}
