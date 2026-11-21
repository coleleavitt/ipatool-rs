//! Port of Go `profilegen/swift_metadata.go`: Swift metadata initialized by the guest
//! runtime, keyed for the Store bridge.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use ipatool_store::profile::{DictionaryLayout, Profile};
use unicorn_engine::RegisterARM64;

use crate::discover::SwiftRequests;
use crate::error::{Error, Result, Wrap};
use crate::vm::{Guest, Vm};

const DATA_SYMBOLS: [&str; 10] = [
    "$ss11AnyHashableVN",
    "$sSSN",
    "$sSiN",
    "$sSbN",
    "$sSdN",
    "$sypN",
    "_swift_stdlib_Hashing_parameters",
    "_swiftEmptyDictionarySingleton",
    "_swiftEmptyArrayStorage",
    "_swiftEmptySetSingleton",
];
const BRIDGED_CLASSES: [&str; 7] = [
    "NSString",
    "NSNumber",
    "NSData",
    "NSDictionary",
    "NSArray",
    "NSObject",
    "NSNull",
];

pub(crate) fn new_metadata_profile() -> Profile {
    Profile::default()
}

const DECODE: &str = "decode Swift metadata requests";

/// Go's `swiftMetadata`.
pub(crate) fn swift_metadata(vm: &mut Vm<'_, '_>, requests: &SwiftRequests) -> Result<Profile> {
    for name in DATA_SYMBOLS {
        let address = export(vm, &format!("_{name}"));
        if address == 0 {
            return Err(Error::failed(format!("missing Swift data symbol {name}")));
        }
        vm.get_data_mut()
            .metadata
            .symbols
            .insert(name.to_owned(), address);
    }

    for request in &requests.types {
        let (address, _) = vm
            .call(
                "swift_getTypeByMangledNameInContext2",
                &[request.address, request.length, 0, 0],
            )
            .wrap(format!(
                "initialize Swift type {:#x}/{}",
                request.address, request.length
            ))?;
        if address == 0 {
            return Err(Error::failed(format!(
                "unresolved Swift type {:#x}",
                request.address
            )));
        }
        vm.get_data_mut()
            .metadata
            .swift_types
            .insert(request.address.to_string(), address);
    }

    for request in &requests.nominals {
        let (address, state) = vm.call_address(request.address, &[0]).wrap(format!(
            "initialize Swift nominal {:#x}",
            request.descriptor
        ))?;
        if address == 0 || state != 0 {
            return Err(Error::failed(format!(
                "incomplete Swift nominal {:#x}",
                request.descriptor
            )));
        }
    }

    for request in &requests.globals {
        let (address, _) = vm.call_address(request.address, &[]).wrap(DECODE)?;
        if address == 0 {
            return Err(Error::failed(format!(
                "unresolved Swift global {}",
                request.name
            )));
        }
        vm.get_data_mut()
            .metadata
            .swift_globals
            .insert(request.name.clone(), address);
    }

    any_hashable_bridge(vm)?;

    for name in BRIDGED_CLASSES {
        let class = vm
            .get_data()
            .source
            .classes
            .values()
            .filter(|class| class.name == name)
            .map(|class| class.address)
            .min()
            .unwrap_or_default();
        let (address, _) = vm
            .call("swift_getObjCClassMetadata", &[class])
            .wrap(DECODE)?;
        if address == 0 {
            return Err(Error::failed(format!("missing ObjC metadata {name}")));
        }
        vm.get_data_mut()
            .metadata
            .objc_metadata
            .insert(name.to_owned(), address);
    }

    for name in &requests.accessors {
        let (address, state) = vm.call(name, &[0]).wrap(DECODE)?;
        if address == 0 || state != 0 {
            return Err(Error::failed(format!("incomplete accessor {name}")));
        }
        vm.get_data_mut()
            .metadata
            .swift_accessors
            .insert(name.clone(), address);
    }

    dictionary_layouts(vm, requests)?;
    prepare_witnesses(vm, requests)?;
    Ok(std::mem::take(&mut vm.get_data_mut().metadata))
}

fn export(vm: &Vm<'_, '_>, name: &str) -> u64 {
    vm.get_data()
        .source
        .exports
        .get(name)
        .copied()
        .unwrap_or_default()
}

fn any_hashable_bridge(vm: &mut Vm<'_, '_>) -> Result<()> {
    let (box_name, protocol_name) = ("s20_ConcreteHashableBoxVySSG", "s15_AnyHashableBox_p");
    let text = vm.cstring(box_name)?;
    let (boxed, _) = vm
        .call(
            "swift_getTypeByMangledNameInContext2",
            &[text, box_name.len() as u64, 0, 0],
        )
        .wrap(DECODE)?;
    let text = vm.cstring(protocol_name)?;
    let (protocol, _) = vm
        .call(
            "swift_getTypeByMangledNameInContext2",
            &[text, protocol_name.len() as u64, 0, 0],
        )
        .wrap(DECODE)?;
    let descriptor = vm.u64(protocol + 16)?;
    let (witness, _) = vm
        .call("swift_conformsToProtocol", &[boxed, descriptor])
        .wrap(DECODE)?;
    if boxed == 0 || witness == 0 {
        return Err(Error::failed("missing AnyHashable bridge"));
    }
    let bridges = &mut vm.get_data_mut().metadata.bridges;
    bridges.insert("anyHashableStringMetadata".to_owned(), boxed);
    bridges.insert("anyHashableStringWitness".to_owned(), witness);
    Ok(())
}

fn dictionary_layouts(vm: &mut Vm<'_, '_>, requests: &SwiftRequests) -> Result<()> {
    let descriptor = export(vm, "_$ss17_NativeDictionaryVMn");
    if descriptor == 0 {
        return Err(Error::failed("missing Swift native dictionary descriptor"));
    }
    for request in &requests.types {
        let metadata = vm
            .get_data()
            .metadata
            .swift_types
            .get(&request.address.to_string())
            .copied()
            .unwrap_or_default();
        if vm.u64(metadata + 8)? != descriptor {
            continue;
        }
        let slot = vm.alloc(8)?;
        let empty = vm.get_data().metadata.symbols["_swiftEmptyDictionarySingleton"];
        vm.put(slot, empty)?;
        vm.write_register(RegisterARM64::X20, slot)?;
        vm.call(
            "$ss17_NativeDictionaryV12ensureUnique02isD08capacityS2b_SitF",
            &[0, 512, metadata],
        )
        .wrap(DECODE)?;
        let storage = vm.u64(slot)?;
        let (key_type, value_type) = (vm.u64(metadata + 16)?, vm.u64(metadata + 24)?);
        let layout = DictionaryLayout {
            metadata: vm.u64(storage)?,
            key_stride: {
                let witnesses = vm.u64(key_type - 8)?;
                vm.u64(witnesses + 72)?
            },
            value_stride: {
                let witnesses = vm.u64(value_type - 8)?;
                vm.u64(witnesses + 72)?
            },
        };
        vm.get_data_mut()
            .metadata
            .swift_dictionaries
            .insert(metadata.to_string(), layout);
    }
    Ok(())
}

/// Go's `prepareWitnesses`: instantiates every unconditional conformance for each known
/// metadata of its type, so the guest runtime records the witness tables.
fn prepare_witnesses(vm: &mut Vm<'_, '_>, requests: &SwiftRequests) -> Result<()> {
    let mut by_descriptor: HashMap<u64, BTreeSet<u64>> = HashMap::new();
    let known: Vec<u64> = {
        let metadata = &vm.get_data().metadata;
        [
            &metadata.swift_types,
            &metadata.swift_accessors,
            &metadata.swift_singletons,
            &metadata.swift_foreign,
        ]
        .into_iter()
        .flat_map(BTreeMap::values)
        .copied()
        .collect()
    };
    for address in known {
        add_instance(vm, &mut by_descriptor, address)?;
    }
    for request in &requests.nominals {
        let (address, _) = vm.call_address(request.address, &[0])?;
        add_instance(vm, &mut by_descriptor, address)?;
    }

    for &conformance in &requests.conformances {
        let source = &vm.get_data().source;
        let flags = source.u32(conformance + 12)?;
        // Conditional conformances need caller-supplied evidence and cannot be initialized
        // from a nominal descriptor alone.
        if flags & 0xff00 != 0 || flags & 0xff00_0000 != 0 {
            continue;
        }
        let kind = (flags >> 3) & 7;
        if kind > 1 {
            continue;
        }
        let mut descriptor = source.relative(conformance + 4)?;
        if kind == 1 {
            descriptor = source.u64(descriptor)?;
        }
        if source.u32(descriptor)? & 0x80 == 0
            && by_descriptor
                .get(&descriptor)
                .is_none_or(BTreeSet::is_empty)
            && source.u32(descriptor + 12)? != 0
        {
            let accessor = source.relative(descriptor + 12)?;
            let (address, state) = vm
                .call_address(accessor, &[0])
                .wrap(format!("initialize conformance type {descriptor:#x}"))?;
            if address == 0 || state != 0 {
                return Err(Error::failed(format!(
                    "incomplete conformance type {descriptor:#x}"
                )));
            }
            add_instance(vm, &mut by_descriptor, address)?;
        }
        let instances: Vec<u64> = by_descriptor
            .get(&descriptor)
            .map_or_else(Vec::new, |set| set.iter().copied().collect());
        for instance in instances {
            let (witness, _) = vm
                .call("swift_getWitnessTable", &[conformance, instance, 0])
                .wrap(format!(
                    "initialize witness {conformance:#x} for {instance:#x}"
                ))?;
            if witness == 0 {
                return Err(Error::failed(format!(
                    "unresolved witness {conformance:#x} for {instance:#x}"
                )));
            }
        }
    }
    Ok(())
}

fn add_instance(
    vm: &mut Vm<'_, '_>,
    by_descriptor: &mut HashMap<u64, BTreeSet<u64>>,
    address: u64,
) -> Result<()> {
    let kind = vm.u64(address)?;
    let descriptor = if kind == 0x200 || kind == 0x201 {
        vm.u64(address + 8)?
    } else if kind > 0x7ff {
        vm.u64(address + 64)?
    } else {
        0
    };
    if descriptor != 0 {
        by_descriptor.entry(descriptor).or_default().insert(address);
    }
    Ok(())
}
