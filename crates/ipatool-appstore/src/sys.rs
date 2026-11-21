//! The operating-system calls Go reaches through its runtime, `golang.org/x/sys` and
//! `purego`: process liveness for the cookie-jar lock, and the adapter list and
//! hardware addresses behind the machine identity. This is the crate's only module
//! that may use `unsafe`.
#![allow(unsafe_code)]

/// Go's `os.FindProcess(pid)` followed by `Signal(0)`: any failure means the owner is gone.
#[cfg(unix)]
#[cfg_attr(
    any(
        all(target_os = "linux", any(target_arch = "x86_64", target_arch = "arm")),
        all(target_os = "macos", target_arch = "x86_64"),
        target_os = "freebsd"
    ),
    allow(dead_code, reason = "the fcntl lock never checks an owner")
)]
pub(crate) fn process_alive(pid: i64) -> bool {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

/// Go's Windows `os.FindProcess`: the process exists when `OpenProcess` succeeds
/// (there is no signal 0 on Windows).
#[cfg(windows)]
pub(crate) fn process_alive(pid: i64) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_SYNCHRONIZE,
    };
    const STANDARD_RIGHTS_READ: u32 = 0x0002_0000;

    let Ok(pid) = u32::try_from(pid) else {
        return false;
    };
    // SAFETY: OpenProcess takes no pointers; it returns null or a handle we own.
    let handle = unsafe {
        OpenProcess(
            STANDARD_RIGHTS_READ | PROCESS_QUERY_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return false;
    }
    // SAFETY: the handle came from OpenProcess and is closed once.
    unsafe { CloseHandle(handle) };
    true
}

/// Go's BSD `net.Interfaces`: one entry per interface name, its flags, and the index
/// and hardware address of its link-layer (`AF_LINK`) address.
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub(crate) fn interfaces() -> std::io::Result<Vec<crate::machine::Interface>> {
    use std::ffi::CStr;

    use crate::machine::Interface;

    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs only writes the list head, which is freed below.
    if unsafe { libc::getifaddrs(&raw mut head) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut interfaces: Vec<Interface> = Vec::new();
    let mut entry = head.cast_const();
    while !entry.is_null() {
        // SAFETY: `entry` is a node of the list getifaddrs returned, not yet freed.
        let node = unsafe { &*entry };
        entry = node.ifa_next.cast_const();
        // SAFETY: getifaddrs gives every node a NUL-terminated name.
        let name = unsafe { CStr::from_ptr(node.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let index = if let Some(index) = interfaces.iter().position(|known| known.name == name) {
            index
        } else {
            interfaces.push(Interface {
                name,
                flags: node.ifa_flags,
                ..Interface::default()
            });
            interfaces.len() - 1
        };
        if let Some((link_index, address)) = link_address(node.ifa_addr) {
            interfaces[index].index = link_index;
            interfaces[index].address = address;
        }
    }
    // SAFETY: `head` came from getifaddrs and no node is used after this.
    unsafe { libc::freeifaddrs(head) };
    Ok(interfaces)
}

/// The interface index and the `sdl_alen` address bytes of a `sockaddr_dl`.
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn link_address(address: *const libc::sockaddr) -> Option<(u32, Vec<u8>)> {
    if address.is_null() {
        return None;
    }
    // SAFETY: a non-null ifa_addr points to a sockaddr of at least its header.
    if i32::from(unsafe { (*address).sa_family }) != libc::AF_LINK {
        return None;
    }
    let link = address.cast::<libc::sockaddr_dl>();
    // SAFETY: an AF_LINK address is a sockaddr_dl, `sdl_len` bytes long.
    let (length, index, name_length, address_length) = unsafe {
        (
            usize::from((*link).sdl_len),
            (*link).sdl_index,
            usize::from((*link).sdl_nlen),
            usize::from((*link).sdl_alen),
        )
    };
    let start = std::mem::offset_of!(libc::sockaddr_dl, sdl_data) + name_length;
    if start + address_length > length {
        return Some((u32::from(index), Vec::new()));
    }
    // SAFETY: the name and the address follow each other in `sdl_data`, inside the
    // `sdl_len` bytes checked above (`sdl_data` is declared shorter than it can be).
    let bytes = unsafe { std::slice::from_raw_parts(link.cast::<u8>().add(start), address_length) };
    Some((u32::from(index), bytes.to_vec()))
}

/// The `IOKit` and `CoreFoundation` calls of Go's `hardwareMacAddress`. Go loads the
/// frameworks with `purego.Dlopen`; here they are linked.
#[cfg(target_os = "macos")]
pub(crate) mod iokit {
    use std::ffi::{CString, c_char};

    use crate::machine::{CfRef, IoKit};

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOBSDNameMatching(main_port: u32, options: u32, bsd_name: *const c_char) -> CfRef;
        fn IOServiceGetMatchingService(main_port: u32, matching: CfRef) -> u32;
        fn IORegistryEntrySearchCFProperty(
            entry: u32,
            plane: *const c_char,
            key: CfRef,
            allocator: CfRef,
            options: u32,
        ) -> CfRef;
        fn IOObjectRelease(object: u32) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: CfRef,
            value: *const c_char,
            encoding: u32,
        ) -> CfRef;
        fn CFRelease(object: CfRef);
        fn CFGetTypeID(object: CfRef) -> usize;
        fn CFDataGetTypeID() -> usize;
        fn CFDataGetLength(data: CfRef) -> isize;
        fn CFDataGetBytePtr(data: CfRef) -> *const u8;
    }

    pub(crate) struct System;

    // `read_hardware_mac_address` passes these functions only objects they returned
    // (null checked first) and releases each once.
    impl IoKit for System {
        fn matching(&self, name: &str) -> CfRef {
            let Ok(name) = CString::new(name) else {
                return std::ptr::null();
            };
            // SAFETY: `name` is a NUL-terminated string that outlives the call.
            unsafe { IOBSDNameMatching(0, 0, name.as_ptr()) }
        }

        fn service(&self, matching: CfRef) -> u32 {
            // SAFETY: `matching` is a dictionary from IOBSDNameMatching, which this call
            // consumes.
            unsafe { IOServiceGetMatchingService(0, matching) }
        }

        fn search_property(&self, service: u32, plane: &str, key: CfRef, options: u32) -> CfRef {
            let Ok(plane) = CString::new(plane) else {
                return std::ptr::null();
            };
            // SAFETY: `service` and `key` are live objects; `plane` outlives the call.
            unsafe {
                IORegistryEntrySearchCFProperty(
                    service,
                    plane.as_ptr(),
                    key,
                    std::ptr::null(),
                    options,
                )
            }
        }

        fn release_object(&self, object: u32) {
            // SAFETY: `object` came from IOServiceGetMatchingService, released once.
            unsafe { IOObjectRelease(object) };
        }

        fn create_string(&self, value: &str, encoding: u32) -> CfRef {
            let Ok(value) = CString::new(value) else {
                return std::ptr::null();
            };
            // SAFETY: `value` is a NUL-terminated string that outlives the call.
            unsafe { CFStringCreateWithCString(std::ptr::null(), value.as_ptr(), encoding) }
        }

        fn release(&self, object: CfRef) {
            // SAFETY: `object` is a non-null object this code owns, released once.
            unsafe { CFRelease(object) };
        }

        fn type_id(&self, object: CfRef) -> usize {
            // SAFETY: `object` is a live, non-null CoreFoundation object.
            unsafe { CFGetTypeID(object) }
        }

        fn data_type_id(&self) -> usize {
            // SAFETY: takes no arguments.
            unsafe { CFDataGetTypeID() }
        }

        fn data_length(&self, data: CfRef) -> isize {
            // SAFETY: the caller checked that `data` is a CFData.
            unsafe { CFDataGetLength(data) }
        }

        fn data_bytes(&self, data: CfRef, length: usize) -> Option<Vec<u8>> {
            // SAFETY: the caller checked that `data` is a CFData.
            let bytes = unsafe { CFDataGetBytePtr(data) };
            if bytes.is_null() {
                return None;
            }
            // SAFETY: the caller checked CFDataGetLength(data) == length, and the
            // bytes are copied while `data` is alive.
            Some(unsafe { std::slice::from_raw_parts(bytes, length) }.to_vec())
        }
    }
}

#[cfg(windows)]
pub(crate) mod windows {
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR, WIN32_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_INCLUDE_PREFIX, GetAdaptersAddresses, IF_TYPE_ATM,
        IF_TYPE_PPP, IF_TYPE_SOFTWARE_LOOPBACK, IF_TYPE_TUNNEL, IP_ADAPTER_ADDRESSES_LH,
        MIB_IF_ENTRY_LEVEL, MIB_IF_ROW2, MibIfEntryNormalWithoutStatistics,
    };
    use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };

    use crate::machine::{FLAG_LOOPBACK, FLAG_POINT_TO_POINT, IfRow, Interface};

    /// Go's Windows `net.Interfaces` (`interfaceTable`, `adapterAddresses`).
    pub(crate) fn interfaces() -> std::io::Result<Vec<Interface>> {
        let mut size: u32 = 15000;
        let mut buffer: Vec<u64>;
        loop {
            buffer = vec![0; (size as usize).div_ceil(8)];
            // SAFETY: the buffer is `size` bytes (rounded up), 8-byte aligned as the
            // structures need, and outlives the walk below.
            let result = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_UNSPEC),
                    GAA_FLAG_INCLUDE_PREFIX | GAA_FLAG_INCLUDE_GATEWAYS,
                    std::ptr::null(),
                    buffer.as_mut_ptr().cast(),
                    &raw mut size,
                )
            };
            if result == NO_ERROR {
                if size == 0 {
                    return Ok(Vec::new());
                }
                break;
            }
            if result != ERROR_BUFFER_OVERFLOW || size as usize <= buffer.len() * 8 {
                return Err(adapter_error(result));
            }
        }
        let mut interfaces = Vec::new();
        let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !adapter.is_null() {
            // SAFETY: GetAdaptersAddresses filled the buffer with a linked list of
            // adapters whose pointers stay inside it.
            let entry = unsafe { &*adapter };
            adapter = entry.Next.cast_const();
            interfaces.push(interface(entry));
        }
        Ok(interfaces)
    }

    fn adapter_error(code: WIN32_ERROR) -> std::io::Error {
        let error = std::io::Error::from_raw_os_error(code.cast_signed());
        std::io::Error::new(error.kind(), format!("getadaptersaddresses: {error}"))
    }

    fn interface(entry: &IP_ADAPTER_ADDRESSES_LH) -> Interface {
        // SAFETY: the union's IfIndex view is the documented layout since XP.
        let mut index = unsafe { entry.Anonymous1.Anonymous.IfIndex };
        if index == 0 {
            index = entry.Ipv6IfIndex;
        }
        let flags = match entry.IfType {
            IF_TYPE_PPP | IF_TYPE_TUNNEL | IF_TYPE_ATM => FLAG_POINT_TO_POINT,
            IF_TYPE_SOFTWARE_LOOPBACK => FLAG_LOOPBACK,
            _ => 0,
        };
        let length = entry.PhysicalAddressLength as usize;
        let mut address = vec![0; length];
        let copied = length.min(entry.PhysicalAddress.len());
        address[..copied].copy_from_slice(&entry.PhysicalAddress[..copied]);
        Interface {
            name: wide_string(entry.FriendlyName),
            index,
            flags,
            address,
        }
    }

    fn wide_string(value: *const u16) -> String {
        if value.is_null() {
            return String::new();
        }
        let mut length = 0;
        // SAFETY: FriendlyName is a NUL-terminated UTF-16 string inside the buffer.
        while unsafe { *value.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: the `length` units before the terminator were just read.
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(value, length) })
    }

    type GetIfEntry2Ex =
        unsafe extern "system" fn(MIB_IF_ENTRY_LEVEL, *mut MIB_IF_ROW2) -> WIN32_ERROR;

    /// `windows.NewLazySystemDLL("iphlpapi.dll").NewProc("GetIfEntry2Ex")`: older
    /// Windows 10 builds lack the entry point, so it is looked up, not linked.
    fn get_if_entry2_ex() -> Option<GetIfEntry2Ex> {
        static PROC: OnceLock<Option<GetIfEntry2Ex>> = OnceLock::new();
        *PROC.get_or_init(|| {
            let library: Vec<u16> = "iphlpapi.dll\0".encode_utf16().collect();
            // SAFETY: the name is NUL-terminated; the module stays loaded for the process.
            let module = unsafe {
                LoadLibraryExW(
                    library.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            };
            if module.is_null() {
                return None;
            }
            // SAFETY: the module handle is valid and the name is NUL-terminated.
            let proc = unsafe { GetProcAddress(module, c"GetIfEntry2Ex".as_ptr().cast()) }?;
            // SAFETY: GetIfEntry2Ex has this signature (netioapi.h).
            Some(unsafe {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, GetIfEntry2Ex>(proc)
            })
        })
    }

    /// `GetIfEntry2Ex(MibIfEntryNormalWithoutStatistics)` for one interface index;
    /// `None` when the entry point is missing or the call fails.
    pub(crate) fn if_row(index: u32) -> Option<IfRow> {
        let get = get_if_entry2_ex()?;
        // SAFETY: MIB_IF_ROW2 is plain data; all zeros is a valid value.
        let mut row: MIB_IF_ROW2 = unsafe { std::mem::zeroed() };
        row.InterfaceIndex = index;
        // SAFETY: `row` is a valid, writable MIB_IF_ROW2 for the call's duration.
        if unsafe { get(MibIfEntryNormalWithoutStatistics, &raw mut row) } != NO_ERROR {
            return None;
        }
        Some(IfRow {
            physical_address_length: row.PhysicalAddressLength,
            physical_address: row.PhysicalAddress,
            permanent_physical_address: row.PermanentPhysicalAddress,
            interface_and_oper_status_flags: row.InterfaceAndOperStatusFlags._bitfield,
            physical_medium_type: row.PhysicalMediumType.cast_unsigned(),
            if_type: row.Type,
        })
    }
}
