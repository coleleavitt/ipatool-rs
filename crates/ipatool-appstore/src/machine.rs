//! Machine identity from the network adapters (`pkg/util/machine`, `machine_id.go`).
//! Apple keys the bag GUID, anisette and kbsync state to it, so it must match the Go
//! tool on the same machine or the shared login state stops working.

use std::path::Path;
use std::sync::OnceLock;

use crate::{Error, Result};

const FLAG_LOOPBACK: u32 = 0x8;
const FLAG_POINT_TO_POINT: u32 = 0x10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Priority {
    #[allow(
        dead_code,
        reason = "the macOS and Windows primary-adapter lookups are not ported yet"
    )]
    Primary,
    Physical,
    Fallback,
}

#[derive(Clone, Debug)]
struct Interface {
    name: String,
    flags: u32,
    address: Vec<u8>,
}

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
    lookup: impl Fn(&Interface) -> std::result::Result<(Vec<u8>, Priority), String>,
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

#[cfg(target_os = "linux")]
fn interfaces(root: &Path) -> std::io::Result<Vec<Interface>> {
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
        });
    }
    Ok(interfaces)
}

/// Go's Linux `interfaceMacAddress`: adapters with a backing sysfs device are physical.
#[cfg(target_os = "linux")]
fn mac_address_from(root: &Path) -> Result<String> {
    let interfaces = interfaces(root)
        .map_err(|err| Error::Invalid(format!("failed to get network interfaces: {err}")))?;
    select(&interfaces, |interface| {
        let priority = if root.join(&interface.name).join("device").exists() {
            Priority::Physical
        } else {
            Priority::Fallback
        };
        Ok((interface.address.clone(), priority))
    })
}

#[cfg(not(target_os = "linux"))]
fn mac_address_from(_root: &Path) -> Result<String> {
    Err(Error::NotPorted(
        "machine MAC address lookup on this platform",
    ))
}

/// The machine's MAC address, read once per process like Go's `machine.MacAddress`.
///
/// # Errors
/// No usable adapter, or the platform lookup is not ported.
pub fn mac_address() -> Result<String> {
    static ADDRESS: OnceLock<std::result::Result<String, String>> = OnceLock::new();
    ADDRESS
        .get_or_init(|| {
            mac_address_from(Path::new("/sys/class/net")).map_err(|err| err.to_string())
        })
        .clone()
        .map_err(Error::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(name: &str, flags: u32, address: &[u8]) -> Interface {
        Interface {
            name: name.to_owned(),
            flags,
            address: address.to_vec(),
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
        assert_eq!(mac_address_from(root.path()).unwrap(), "00:16:3e:12:34:56");
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
