//! Machine identity from the network adapters (`pkg/util/machine`, `machine_id.go`).
//! Apple keys the bag GUID, anisette and kbsync state to it, so it must match the Go
//! tool on the same machine or the shared login state stops working.
//!
//! The adapter list and the per-OS address lookups follow Go's build files:
//! `mac_address_linux.go` (sysfs backing device), `mac_address_darwin.go` (`IOKit`
//! `IOMACAddress`, `en0` first), `mac_address_windows.go` (`GetIfEntry2Ex` permanent
//! address) and `mac_address_default.go` (the interface address, also on iOS).

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::path::Path;
use std::sync::OnceLock;

use crate::{Error, Result};

/// `IFF_LOOPBACK` and `IFF_POINTOPOINT` (the same bits on Linux and the BSDs); the
/// Windows adapter list sets them from the interface type as Go does.
pub(crate) const FLAG_LOOPBACK: u32 = 0x8;
pub(crate) const FLAG_POINT_TO_POINT: u32 = 0x10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Priority {
    #[cfg_attr(
        not(target_os = "macos"),
        allow(dead_code, reason = "only the macOS lookup has a primary interface")
    )]
    Primary,
    #[cfg_attr(
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            windows
        )),
        allow(dead_code, reason = "the default lookup has fallbacks only")
    )]
    Physical,
    Fallback,
}

/// The fields of Go's `net.Interface` the selection reads.
#[derive(Clone, Debug, Default)]
pub(crate) struct Interface {
    #[cfg_attr(
        windows,
        allow(dead_code, reason = "the Windows row lookup goes by index")
    )]
    pub name: String,
    #[cfg_attr(
        not(windows),
        allow(dead_code, reason = "only the Windows row lookup reads the index")
    )]
    pub index: u32,
    pub flags: u32,
    pub address: Vec<u8>,
}

/// Go's `macAddressCandidate`.
type Candidate = (Vec<u8>, Priority);

/// Go's `usableMacAddress`: a unicast EUI-48 that is not all zeros or redacted.
fn usable(address: &[u8]) -> bool {
    address.len() == 6
        && address[0] & 1 == 0
        && address != [0, 0, 0, 0, 0, 0]
        && address != [2, 0, 0, 0, 0, 0]
}

/// Go's `selectMacAddress`: best priority, then the smallest address. Enumeration
/// order, interface index and link state change without the machine changing.
fn select(
    interfaces: &[Interface],
    lookup: impl Fn(&Interface) -> std::result::Result<Candidate, String>,
) -> Result<String> {
    let mut selected: Option<(Priority, Vec<u8>)> = None;
    let mut lookup_errors = Vec::new();
    for interface in interfaces {
        if interface.flags & (FLAG_LOOPBACK | FLAG_POINT_TO_POINT) != 0 {
            continue;
        }
        let (address, priority) = match lookup(interface) {
            Ok(candidate) => candidate,
            Err(err) => {
                lookup_errors.push(err);
                continue;
            }
        };
        if !usable(&address) {
            continue;
        }
        let candidate = (priority, address);
        if selected.as_ref().is_none_or(|best| candidate < *best) {
            selected = Some(candidate);
        }
    }
    if let Some((_, address)) = selected {
        return Ok(format_mac(&address));
    }
    let message = "could not find network interfaces with a valid mac address";
    if lookup_errors.is_empty() {
        Err(Error::Invalid(message.to_owned()))
    } else {
        Err(Error::Invalid(format!(
            "{message}: {}",
            lookup_errors.join("\n")
        )))
    }
}

/// Go's `machine.MacAddress` body: list the adapters, then select.
fn resolve(
    interfaces: std::io::Result<Vec<Interface>>,
    lookup: impl Fn(&Interface) -> std::result::Result<Candidate, String>,
) -> Result<String> {
    let interfaces = interfaces
        .map_err(|err| Error::Invalid(format!("failed to get network interfaces: {err}")))?;
    select(&interfaces, lookup)
}

fn format_mac(address: &[u8]) -> String {
    address
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Go's `net.ParseMAC` for the colon form, which is all `selectMacAddress` produces.
fn parse_mac(value: &str) -> Option<Vec<u8>> {
    let bytes = value
        .split(':')
        .map(|part| {
            (part.len() == 2)
                .then(|| u8::from_str_radix(part, 16).ok())
                .flatten()
        })
        .collect::<Option<Vec<u8>>>()?;
    matches!(bytes.len(), 6 | 8 | 20).then_some(bytes)
}

/// Go's `machineIdentity`: the bag GUID (upper-case hex) and the raw machine ID.
///
/// # Errors
/// The address is not a colon-separated EUI-48, EUI-64 or 20-byte InfiniBand address.
pub fn identity(mac_address: &str) -> Result<(String, Vec<u8>)> {
    let bytes = parse_mac(mac_address).ok_or_else(|| {
        Error::Invalid(format!(
            "failed to parse mac address: address {mac_address}: invalid MAC address"
        ))
    })?;
    let guid = bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .concat();
    Ok((guid, bytes))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const SYSFS_NET: &str = "/sys/class/net";

#[cfg(any(target_os = "linux", target_os = "android"))]
fn sysfs_interfaces(root: &Path) -> std::io::Result<Vec<Interface>> {
    let read = |path: &Path| std::fs::read_to_string(path).map(|value| value.trim().to_owned());
    let mut interfaces = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let flags = read(&path.join("flags"))
            .ok()
            .and_then(|flags| u32::from_str_radix(flags.trim_start_matches("0x"), 16).ok())
            .unwrap_or_default();
        let address = read(&path.join("address"))
            .ok()
            .and_then(|address| parse_mac(&address))
            .unwrap_or_default();
        interfaces.push(Interface {
            name: entry.file_name().to_string_lossy().into_owned(),
            flags,
            address,
            ..Interface::default()
        });
    }
    Ok(interfaces)
}

/// Go's `macAddressWithSysfs`: adapters with a backing sysfs device are physical; the
/// rest stay as fallbacks for containers and systems without sysfs.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn sysfs_candidate(interface: &Interface, root: &Path) -> Candidate {
    let priority = if root.join(&interface.name).join("device").exists() {
        Priority::Physical
    } else {
        Priority::Fallback
    };
    (interface.address.clone(), priority)
}

/// Go's `macAddressWithHardwareLookup`: `IOKit` supplies the hardware address even when
/// the network address is randomized or redacted. `en0` first, then other hardware
/// interfaces, then an unredacted network address.
#[cfg(any(target_os = "macos", test))]
fn with_hardware_lookup(
    interface: &Interface,
    lookup: impl Fn(&str) -> std::result::Result<Vec<u8>, String>,
) -> std::result::Result<Candidate, String> {
    let address = lookup(&interface.name);
    if let Ok(address) = &address
        && usable(address)
    {
        let priority = if interface.name == "en0" {
            Priority::Primary
        } else {
            Priority::Physical
        };
        return Ok((address.clone(), priority));
    }
    if usable(&interface.address) {
        return Ok((interface.address.clone(), Priority::Fallback));
    }
    Err(match address {
        Err(err) => format!(
            "failed to read hardware address for {:?}: {err}",
            interface.name
        ),
        Ok(_) => format!(
            "IOKit returned no usable hardware address for {:?}",
            interface.name
        ),
    })
}

/// A CoreFoundation object (`CFTypeRef`); null is Go's `0`.
#[cfg(any(target_os = "macos", test))]
pub(crate) type CfRef = *const std::ffi::c_void;

/// Go's `macAddressAPI`: the `IOKit` and `CoreFoundation` functions
/// `readHardwareMacAddress` calls, so tests can check what is released.
#[cfg(any(target_os = "macos", test))]
pub(crate) trait IoKit {
    /// `IOBSDNameMatching(kIOMainPortDefault, 0, name)`.
    fn matching(&self, name: &str) -> CfRef;
    /// `IOServiceGetMatchingService(kIOMainPortDefault, matching)`; consumes `matching`.
    fn service(&self, matching: CfRef) -> u32;
    /// `IORegistryEntrySearchCFProperty(service, plane, key, NULL, options)`.
    fn search_property(&self, service: u32, plane: &str, key: CfRef, options: u32) -> CfRef;
    /// `IOObjectRelease`.
    fn release_object(&self, object: u32);
    /// `CFStringCreateWithCString(NULL, value, encoding)`.
    fn create_string(&self, value: &str, encoding: u32) -> CfRef;
    /// `CFRelease`.
    fn release(&self, object: CfRef);
    /// `CFGetTypeID`.
    fn type_id(&self, object: CfRef) -> usize;
    /// `CFDataGetTypeID`.
    fn data_type_id(&self) -> usize;
    /// `CFDataGetLength`.
    fn data_length(&self, data: CfRef) -> isize;
    /// A copy of the first `length` bytes of `CFDataGetBytePtr`; `None` when it is null.
    fn data_bytes(&self, data: CfRef, length: usize) -> Option<Vec<u8>>;
}

/// Go's `readHardwareMacAddress`: `IOMACAddress` of the interface or its parents in
/// the `IOService` plane, as Apple's receipt-validation guidance reads it
/// (<https://developer.apple.com/documentation/appstorereceipts/validating-receipts-on-the-device>).
/// The bytes are copied before the property is released.
#[cfg(any(target_os = "macos", test))]
fn read_hardware_mac_address(name: &str, api: &impl IoKit) -> std::result::Result<Vec<u8>, String> {
    const UTF8_ENCODING: u32 = 0x0800_0100;
    const ITERATE_RECURSIVELY_AND_PARENTS: u32 = 0x01 | 0x02;

    let matching = api.matching(name);
    if matching.is_null() {
        return Err("could not create interface matching dictionary".to_owned());
    }
    let service = api.service(matching);
    if service == 0 {
        return Err("network interface was not found in IOKit".to_owned());
    }
    let address = (|| {
        let key = api.create_string("IOMACAddress", UTF8_ENCODING);
        if key.is_null() {
            return Err("could not create hardware address property key".to_owned());
        }
        let property =
            api.search_property(service, "IOService", key, ITERATE_RECURSIVELY_AND_PARENTS);
        let address = if property.is_null() {
            Err("hardware address property was not found in IOKit".to_owned())
        } else {
            let address =
                if api.type_id(property) != api.data_type_id() || api.data_length(property) != 6 {
                    Err("IOKit hardware address must contain six bytes".to_owned())
                } else {
                    api.data_bytes(property, 6)
                        .ok_or_else(|| "IOKit hardware address data is missing".to_owned())
                };
            api.release(property);
            address
        };
        api.release(key);
        address
    })();
    api.release_object(service);
    address
}

/// Go's `macAddressFromWindowsRow` input: the `MIB_IF_ROW2` fields it reads.
#[cfg(any(windows, test))]
pub(crate) struct IfRow {
    pub physical_address_length: u32,
    pub physical_address: [u8; 32],
    pub permanent_physical_address: [u8; 32],
    pub interface_and_oper_status_flags: u8,
    pub physical_medium_type: u32,
    pub if_type: u32,
}

/// Go's `macAddressFromWindowsRow`.
#[cfg(any(windows, test))]
fn from_windows_row(row: &IfRow) -> Candidate {
    // MIB_IF_ROW2's first two status bits are HardwareInterface and FilterInterface.
    const HARDWARE_INTERFACE: u8 = 1;
    const FILTER_INTERFACE: u8 = 2;
    const BLUETOOTH_MEDIUM: u32 = 10;
    const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
    const IF_TYPE_IEEE80211: u32 = 71;

    if row.physical_address_length != 6 {
        return (Vec::new(), Priority::Fallback);
    }
    // The permanent address, so Wi-Fi randomization does not change the identity.
    let address = if usable(&row.permanent_physical_address[..6]) {
        &row.permanent_physical_address[..6]
    } else {
        &row.physical_address[..6]
    };
    // Bluetooth PAN can otherwise look like Ethernet; virtual adapters stay fallbacks.
    let physical = row.interface_and_oper_status_flags & (HARDWARE_INTERFACE | FILTER_INTERFACE)
        == HARDWARE_INTERFACE
        && row.physical_medium_type != BLUETOOTH_MEDIUM
        && matches!(row.if_type, IF_TYPE_ETHERNET_CSMACD | IF_TYPE_IEEE80211);
    let priority = if physical {
        Priority::Physical
    } else {
        Priority::Fallback
    };
    (address.to_vec(), priority)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn system_mac_address() -> Result<String> {
    let root = Path::new(SYSFS_NET);
    resolve(sysfs_interfaces(root), |interface| {
        Ok(sysfs_candidate(interface, root))
    })
}

#[cfg(target_os = "macos")]
fn system_mac_address() -> Result<String> {
    resolve(crate::sys::interfaces(), |interface| {
        with_hardware_lookup(interface, |name| {
            read_hardware_mac_address(name, &crate::sys::iokit::System)
        })
    })
}

/// Go's Windows `interfaceMacAddress`: without `GetIfEntry2Ex` (older Windows 10) or
/// when it fails, the network address stays as a fallback.
#[cfg(windows)]
fn system_mac_address() -> Result<String> {
    resolve(crate::sys::windows::interfaces(), |interface| {
        Ok(crate::sys::windows::if_row(interface.index).map_or_else(
            || (interface.address.clone(), Priority::Fallback),
            |row| from_windows_row(&row),
        ))
    })
}

/// `mac_address_default.go`, which iOS also uses: the interface address as a fallback.
#[cfg(any(
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn system_mac_address() -> Result<String> {
    resolve(crate::sys::interfaces(), |interface| {
        Ok((interface.address.clone(), Priority::Fallback))
    })
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows,
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
fn system_mac_address() -> Result<String> {
    Err(Error::NotPorted(
        "listing network interfaces on this platform",
    ))
}

/// Resolves once, so bag requests, authentication and downloads use one identity even
/// if an adapter is added or removed during this run.
fn cached(
    cell: &OnceLock<std::result::Result<String, String>>,
    resolve: impl FnOnce() -> Result<String>,
) -> Result<String> {
    cell.get_or_init(|| resolve().map_err(|err| err.to_string()))
        .clone()
        .map_err(Error::Invalid)
}

/// The machine's MAC address, read once per process like Go's `machine.MacAddress`.
///
/// # Errors
/// No usable adapter, or the adapters cannot be listed.
pub fn mac_address() -> Result<String> {
    static ADDRESS: OnceLock<std::result::Result<String, String>> = OnceLock::new();
    cached(&ADDRESS, system_mac_address)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(name: &str, flags: u32, address: &[u8]) -> Interface {
        Interface {
            name: name.to_owned(),
            flags,
            address: address.to_vec(),
            ..Interface::default()
        }
    }

    #[test]
    fn validates_hardware_addresses() {
        for (address, valid) in [
            (&[][..], false),
            (&[0, 1], false),
            (&[0, 1, 2, 3, 4, 5, 6, 7], false),
            (&[0, 0, 0, 0, 0, 0], false),
            (&[2, 0, 0, 0, 0, 0], false),
            (&[255, 255, 255, 255, 255, 255], false),
            (&[1, 0, 0x5e, 0, 0, 1], false),
            (&[0, 1, 2, 3, 4, 5], true),
            (&[2, 1, 2, 3, 4, 5], true),
        ] {
            assert_eq!(usable(address), valid, "{address:?}");
        }
    }

    #[test]
    fn prefers_physical_adapters_regardless_of_order() {
        let mut interfaces = vec![
            interface("virtual", 0, &[0, 0, 0, 0, 0, 1]),
            interface("wired", 0, &[0, 0, 0, 0, 0, 3]),
            interface("wireless", 0, &[0, 0, 0, 0, 0, 2]),
        ];
        let lookup = |interface: &Interface| {
            let priority = if interface.name == "virtual" {
                Priority::Fallback
            } else {
                Priority::Physical
            };
            Ok((interface.address.clone(), priority))
        };
        for _ in 0..3 {
            assert_eq!(select(&interfaces, lookup).unwrap(), "00:00:00:00:00:02");
            interfaces.reverse();
            assert_eq!(select(&interfaces, lookup).unwrap(), "00:00:00:00:00:02");
            interfaces.rotate_left(1);
        }
    }

    #[test]
    fn prefers_primary_over_physical() {
        let interfaces = [interface("other", 0, &[]), interface("primary", 0, &[])];
        let address = select(&interfaces, |interface| {
            Ok(if interface.name == "primary" {
                (vec![0, 0, 0, 0, 0, 2], Priority::Primary)
            } else {
                (vec![0, 0, 0, 0, 0, 1], Priority::Physical)
            })
        })
        .unwrap();
        assert_eq!(address, "00:00:00:00:00:02");
    }

    #[test]
    fn skips_loopback_tunnels_failures_and_unusable_addresses() {
        let interfaces = [
            interface("loopback", FLAG_LOOPBACK, &[0, 1, 2, 3, 4, 5]),
            interface("tunnel", FLAG_POINT_TO_POINT, &[0, 1, 2, 3, 4, 5]),
            interface("unavailable", 0, &[]),
            interface("redacted", 0, &[2, 0, 0, 0, 0, 0]),
            interface("virtual", 0, &[2, 1, 2, 3, 4, 5]),
        ];
        let address = select(&interfaces, |interface| {
            assert_eq!(interface.flags, 0);
            if interface.name == "unavailable" {
                return Err("hardware lookup failed".to_owned());
            }
            Ok((interface.address.clone(), Priority::Fallback))
        })
        .unwrap();
        assert_eq!(address, "02:01:02:03:04:05");
    }

    #[test]
    fn reports_missing_addresses_and_kept_lookup_failures() {
        assert_eq!(
            select(&[], |_| unreachable!()).unwrap_err().to_string(),
            "could not find network interfaces with a valid mac address"
        );
        assert_eq!(
            select(&[interface("en0", 0, &[])], |_| Err(
                "hardware unavailable".to_owned()
            ))
            .unwrap_err()
            .to_string(),
            "could not find network interfaces with a valid mac address: hardware unavailable"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_sysfs_like_go() {
        let root = tempfile::tempdir().unwrap();
        for (name, address, flags, physical) in [
            ("lo", "00:00:00:00:00:00", "0x9", false),
            ("veth0", "02:00:00:00:00:09", "0x1003", false),
            ("eth1", "00:16:3e:65:43:21", "0x1003", true),
            ("eth0", "00:16:3e:12:34:56", "0x1003", true),
        ] {
            let dir = root.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("address"), format!("{address}\n")).unwrap();
            std::fs::write(dir.join("flags"), format!("{flags}\n")).unwrap();
            if physical {
                std::fs::create_dir(dir.join("device")).unwrap();
            }
        }
        let address = resolve(sysfs_interfaces(root.path()), |interface| {
            Ok(sysfs_candidate(interface, root.path()))
        });
        assert_eq!(address.unwrap(), "00:16:3e:12:34:56");
    }

    /// Go's `mac_address_linux_test.go`.
    #[cfg(target_os = "linux")]
    #[test]
    fn prefers_a_sysfs_backing_device() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("eth0/device")).unwrap();
        std::fs::create_dir_all(root.path().join("veth0")).unwrap();
        for (name, priority) in [
            ("eth0", Priority::Physical),
            ("veth0", Priority::Fallback),
            ("missing", Priority::Fallback),
        ] {
            let interface = interface(name, 0, &[2, 1, 2, 3, 4, 5]);
            assert_eq!(
                sysfs_candidate(&interface, root.path()),
                (interface.address.clone(), priority),
                "{name}"
            );
        }
    }

    #[test]
    fn keeps_one_identity_across_concurrent_calls() {
        let cell = OnceLock::new();
        let calls = std::sync::atomic::AtomicU8::new(0);
        let resolve_once = || {
            let last = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            select(
                &[interface("ipatool-test", 0, &[2, 1, 2, 3, 4, last])],
                |interface| Ok((interface.address.clone(), Priority::Fallback)),
            )
        };
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    assert_eq!(cached(&cell, resolve_once).unwrap(), "02:01:02:03:04:01");
                });
            }
        });
        assert_eq!(cached(&cell, resolve_once).unwrap(), "02:01:02:03:04:01");
        assert_eq!(calls.into_inner(), 1);
    }

    #[test]
    fn preserves_interface_enumeration_errors() {
        let err = resolve(
            Err(std::io::Error::other("interfaces unavailable")),
            |_| unreachable!(),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "failed to get network interfaces: interfaces unavailable"
        );
    }

    /// Go's `mac_address_darwin_test.go`, "macOS hardware address".
    mod darwin {
        use super::*;

        fn en4(address: &[u8]) -> Interface {
            interface("en4", 0, address)
        }

        #[test]
        fn falls_back_to_an_unredacted_address_without_iokit() {
            let candidate =
                with_hardware_lookup(&en4(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]), |_| {
                    Err("IOKit unavailable".to_owned())
                })
                .unwrap();
            assert_eq!(
                candidate,
                (vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff], Priority::Fallback)
            );
        }

        #[test]
        fn recovers_the_hardware_address_when_redacted() {
            let candidate = with_hardware_lookup(&en4(&[2, 0, 0, 0, 0, 0]), |name| {
                assert_eq!(name, "en4");
                Ok(vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
            })
            .unwrap();
            assert_eq!(
                candidate,
                (vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff], Priority::Physical)
            );
        }

        #[test]
        fn prefers_en0_hardware_address_over_its_randomized_one() {
            let en0 = interface("en0", 0, &[2, 1, 2, 3, 4, 5]);
            let candidate = with_hardware_lookup(&en0, |name| {
                assert_eq!(name, "en0");
                Ok(vec![0, 1, 2, 3, 4, 5])
            })
            .unwrap();
            assert_eq!(candidate, (vec![0, 1, 2, 3, 4, 5], Priority::Primary));
        }

        #[test]
        fn reports_unavailable_hardware_without_the_placeholder() {
            let err = with_hardware_lookup(&en4(&[2, 0, 0, 0, 0, 0]), |_| {
                Err("interface unavailable".to_owned())
            })
            .unwrap_err();
            assert_eq!(
                err,
                "failed to read hardware address for \"en4\": interface unavailable"
            );
        }

        #[test]
        fn rejects_an_unusable_iokit_address() {
            for hardware in [
                &[][..],
                &[0xaa, 0xbb],
                &[0, 0, 0, 0, 0, 0],
                &[2, 0, 0, 0, 0, 0],
                &[0xff; 6],
                &[1, 0, 0x5e, 0, 0, 1],
            ] {
                let err =
                    with_hardware_lookup(&en4(&[2, 0, 0, 0, 0, 0]), |_| Ok(hardware.to_vec()))
                        .unwrap_err();
                assert_eq!(
                    err, "IOKit returned no usable hardware address for \"en4\"",
                    "{hardware:?}"
                );
            }
        }
    }

    /// Go's `mac_address_darwin_test.go`, "`IOKit` hardware address lookup".
    mod iokit {
        use std::cell::RefCell;

        use super::*;

        fn handle(value: usize) -> CfRef {
            std::ptr::without_provenance(value)
        }

        const SERVICE: u32 = 2;
        const DATA_TYPE: usize = 5;

        struct Fake {
            matching: CfRef,
            service: u32,
            key: CfRef,
            property: CfRef,
            type_id: usize,
            length: Option<isize>,
            data: RefCell<Option<Vec<u8>>>,
            bytes_allowed: bool,
            released: RefCell<Vec<CfRef>>,
            released_objects: RefCell<Vec<u32>>,
        }

        impl Default for Fake {
            fn default() -> Self {
                Self {
                    matching: handle(1),
                    service: SERVICE,
                    key: handle(3),
                    property: handle(4),
                    type_id: DATA_TYPE,
                    length: Some(6),
                    data: RefCell::new(Some(vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])),
                    bytes_allowed: true,
                    released: RefCell::default(),
                    released_objects: RefCell::default(),
                }
            }
        }

        impl IoKit for Fake {
            fn matching(&self, name: &str) -> CfRef {
                assert_eq!(name, "en4");
                self.matching
            }

            fn service(&self, matching: CfRef) -> u32 {
                assert_eq!(matching, handle(1));
                self.service
            }

            fn search_property(
                &self,
                service: u32,
                plane: &str,
                key: CfRef,
                options: u32,
            ) -> CfRef {
                assert_eq!(
                    (service, plane, key, options),
                    (SERVICE, "IOService", handle(3), 3)
                );
                self.property
            }

            fn release_object(&self, object: u32) {
                self.released_objects.borrow_mut().push(object);
            }

            fn create_string(&self, value: &str, encoding: u32) -> CfRef {
                assert_eq!((value, encoding), ("IOMACAddress", 0x0800_0100));
                self.key
            }

            fn release(&self, object: CfRef) {
                self.released.borrow_mut().push(object);
                if object == handle(4) {
                    // Go's test clears the bytes on release to prove they were copied.
                    if let Some(data) = self.data.borrow_mut().as_mut() {
                        data.fill(0);
                    }
                }
            }

            fn type_id(&self, object: CfRef) -> usize {
                assert_eq!(object, handle(4));
                self.type_id
            }

            fn data_type_id(&self) -> usize {
                DATA_TYPE
            }

            fn data_length(&self, data: CfRef) -> isize {
                assert_eq!(data, handle(4));
                self.length
                    .expect("data length read for a non-data property")
            }

            fn data_bytes(&self, data: CfRef, length: usize) -> Option<Vec<u8>> {
                assert_eq!(data, handle(4));
                assert!(self.bytes_allowed, "bytes read for an unexpected length");
                self.data
                    .borrow()
                    .as_ref()
                    .map(|data| data[..length].to_vec())
            }
        }

        fn check(
            fake: &Fake,
            expected: std::result::Result<&[u8], &str>,
            released: &[usize],
            objects: &[u32],
        ) {
            let result = read_hardware_mac_address("en4", fake);
            assert_eq!(result.as_deref().map_err(String::as_str), expected);
            let released: Vec<CfRef> = released.iter().map(|&value| handle(value)).collect();
            assert_eq!(*fake.released.borrow(), released);
            assert_eq!(*fake.released_objects.borrow(), objects);
        }

        #[test]
        fn copies_the_address_before_releasing() {
            check(
                &Fake::default(),
                Ok(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
                &[4, 3],
                &[SERVICE],
            );
        }

        #[test]
        fn handles_a_missing_matching_dictionary() {
            let fake = Fake {
                matching: std::ptr::null(),
                ..Fake::default()
            };
            check(
                &fake,
                Err("could not create interface matching dictionary"),
                &[],
                &[],
            );
        }

        #[test]
        fn does_not_release_the_consumed_dictionary_for_a_missing_interface() {
            let fake = Fake {
                service: 0,
                ..Fake::default()
            };
            check(
                &fake,
                Err("network interface was not found in IOKit"),
                &[],
                &[],
            );
        }

        #[test]
        fn releases_the_interface_when_the_key_cannot_be_created() {
            let fake = Fake {
                key: std::ptr::null(),
                ..Fake::default()
            };
            check(
                &fake,
                Err("could not create hardware address property key"),
                &[],
                &[SERVICE],
            );
        }

        #[test]
        fn releases_the_interface_and_key_when_the_property_is_missing() {
            let fake = Fake {
                property: std::ptr::null(),
                ..Fake::default()
            };
            check(
                &fake,
                Err("hardware address property was not found in IOKit"),
                &[3],
                &[SERVICE],
            );
        }

        #[test]
        fn rejects_a_non_data_property_without_reading_it() {
            let fake = Fake {
                type_id: DATA_TYPE + 1,
                length: None,
                bytes_allowed: false,
                ..Fake::default()
            };
            check(
                &fake,
                Err("IOKit hardware address must contain six bytes"),
                &[4, 3],
                &[SERVICE],
            );
        }

        #[test]
        fn rejects_an_unexpected_length_without_reading_the_bytes() {
            let fake = Fake {
                length: Some(8),
                bytes_allowed: false,
                ..Fake::default()
            };
            check(
                &fake,
                Err("IOKit hardware address must contain six bytes"),
                &[4, 3],
                &[SERVICE],
            );
        }

        #[test]
        fn handles_missing_data() {
            let fake = Fake {
                data: RefCell::new(None),
                ..Fake::default()
            };
            check(
                &fake,
                Err("IOKit hardware address data is missing"),
                &[4, 3],
                &[SERVICE],
            );
        }
    }

    /// Go's `mac_address_windows_test.go`.
    mod windows_row {
        use super::*;

        fn row() -> IfRow {
            let mut row = IfRow {
                physical_address_length: 6,
                physical_address: [0; 32],
                permanent_physical_address: [0; 32],
                interface_and_oper_status_flags: 1,
                physical_medium_type: 0,
                if_type: 6,
            };
            row.physical_address[..6].copy_from_slice(&[2, 1, 2, 3, 4, 5]);
            row.permanent_physical_address[..6].copy_from_slice(&[0, 1, 2, 3, 4, 5]);
            row
        }

        /// Go also varies `OperStatus` here; the lookup never reads it.
        #[test]
        fn uses_a_physical_adapters_permanent_address() {
            assert_eq!(
                from_windows_row(&row()),
                (vec![0, 1, 2, 3, 4, 5], Priority::Physical)
            );
        }

        #[test]
        fn uses_the_current_address_without_a_permanent_one() {
            let mut row = row();
            row.permanent_physical_address = [0; 32];
            assert_eq!(
                from_windows_row(&row),
                (vec![2, 1, 2, 3, 4, 5], Priority::Physical)
            );
        }

        #[test]
        fn classifies_adapters() {
            for (name, flags, if_type, medium, priority) in [
                ("Wi-Fi hardware", 1, 71, 9, Priority::Physical),
                ("virtual Ethernet", 0, 6, 0, Priority::Fallback),
                ("filter", 3, 6, 0, Priority::Fallback),
                ("Bluetooth PAN", 1, 6, 10, Priority::Fallback),
                ("tunnel", 1, 131, 0, Priority::Fallback),
            ] {
                let mut row = row();
                row.interface_and_oper_status_flags = flags;
                row.if_type = if_type;
                row.physical_medium_type = medium;
                assert_eq!(from_windows_row(&row).1, priority, "{name}");
            }
        }

        #[test]
        fn rejects_non_ethernet_address_lengths() {
            for length in [0, 8, 100] {
                let mut row = row();
                row.physical_address_length = length;
                assert!(from_windows_row(&row).0.is_empty(), "{length}");
            }
        }
    }

    /// Go's `machine_test.go` "returns a usable MAC address", on this machine's adapters.
    #[test]
    fn reads_a_usable_address_from_this_machine() {
        let address = mac_address().unwrap();
        assert!(address.contains(':'), "{address}");
        if cfg!(target_os = "macos") {
            assert_ne!(address, "02:00:00:00:00:00");
        }
    }

    #[test]
    fn derives_machine_identity() {
        for (address, guid, bytes) in [
            (
                "00:11:22:aa:bb:cc",
                "001122AABBCC",
                vec![0x00, 0x11, 0x22, 0xaa, 0xbb, 0xcc],
            ),
            (
                "00:11:22:33:44:55:66:77",
                "0011223344556677",
                (0..8).map(|i| i * 0x11).collect(),
            ),
            (
                "00:01:02:03:04:05:06:07:08:09:0a:0b:0c:0d:0e:0f:10:11:12:13",
                "000102030405060708090A0B0C0D0E0F10111213",
                (0..20).collect(),
            ),
        ] {
            assert_eq!(
                identity(address).unwrap(),
                (guid.to_owned(), bytes),
                "{address}"
            );
        }
        assert!(identity("not-a-hardware-address").is_err());
    }
}
