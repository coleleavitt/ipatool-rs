//! Ports of Go `profilegen_test.go` and `discover_test.go`.

use std::collections::HashMap;
use std::path::PathBuf;

use ipatool_sap::assets::StoreCache;
use ipatool_store::profile::witness_key;
use unicorn_engine::RegisterARM64;

use crate::collect::adrp;
use crate::discover::{
    TypeRequest, concrete_type_records, mangled_name_length, referenced_pointer,
};
use crate::source::{Source, inside};
use crate::vm::{
    BOOT_HEAP, BOOT_HEAP_SIZE, BOOT_RETURN, Guest, State, bootstrap_instruction, new_vm,
};

/// A one-mapping arm64e shared cache without rebases, enough to open a `StoreCache`.
fn synthetic_cache() -> (tempfile::TempDir, PathBuf) {
    let mut data = vec![0u8; 0x5000];
    data[..15].copy_from_slice(b"dyld_v1  arm64e");
    data[312..316].copy_from_slice(&0x400u32.to_le_bytes());
    data[316..320].copy_from_slice(&1u32.to_le_bytes());
    for (index, value) in [0x1_8000_0000u64, 0x4000, 0x1000, 0x800, 26]
        .into_iter()
        .enumerate()
    {
        data[0x400 + index * 8..0x408 + index * 8].copy_from_slice(&value.to_le_bytes());
    }
    data[0x800..0x804].copy_from_slice(&5u32.to_le_bytes());
    data[0x804..0x808].copy_from_slice(&0x4000u32.to_le_bytes());
    data[0x808..0x80c].copy_from_slice(&1u32.to_le_bytes());
    data[0x818..0x81a].copy_from_slice(&0xffffu16.to_le_bytes());
    data[0x1000..0x1004].copy_from_slice(b"page");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("dyld_shared_cache_arm64e");
    std::fs::write(&path, data).unwrap();
    (directory, path)
}

fn with_vm(test: impl FnOnce(&mut crate::vm::Vm<'_, '_>)) {
    let (_directory, path) = synthetic_cache();
    let cache = StoreCache::open_local(&path).unwrap();
    let mut vm = new_vm(Source::empty(&cache)).unwrap();
    test(&mut vm);
}

#[test]
fn reads_a_local_store_cache_by_unslid_address() {
    let (_directory, path) = synthetic_cache();
    let cache = StoreCache::open_local(&path).unwrap();
    assert_eq!(cache.address_range(), (0x1_8000_0000, 0x4000));
    assert!(cache.contains(0x1_8000_3fe0, 32));
    assert!(!cache.contains(0x1_8000_3fe1, 32));
    assert!(!cache.contains(0x1_7fff_fff0, 8));
    // The mapping starts at file offset 0x1000.
    assert_eq!(&cache.page(0x1_8000_0000).unwrap()[..4], b"page");
    assert!(cache.page(0x1_8000_0001).is_err());
    assert_eq!(
        cache.feature_flags("Security").err().unwrap().to_string(),
        "unsupported Store feature domain"
    );
}

#[test]
fn version_matches_the_embedded_profile() {
    assert_eq!(crate::version(), ipatool_store::profile::PINNED_INPUTS);
}

#[test]
fn decodes_negative_adrp_page_displacements() {
    assert_eq!(adrp(0xf0ff_ffe0, 0x200_1234), 0x200_0000);
}

#[test]
fn preserves_ordinary_instructions_and_lowers_rcpc_loads() {
    assert_eq!(bootstrap_instruction(0xf8bf_c115), 0xf940_0115);
    assert_eq!(bootstrap_instruction(0xd503_201f), 0xd503_201f);
}

#[test]
fn rejects_overflowing_guest_heap_allocations() {
    with_vm(|vm| {
        let state = vm.get_data_mut();
        assert!(state.alloc(u64::MAX).is_err());
        state.cursor = BOOT_HEAP_SIZE + 1;
        assert!(state.alloc(16).is_err());
    });
}

#[test]
fn looks_up_sorted_noncontiguous_image_ranges() {
    let ranges = [(8, 16), (24, 40)];
    assert!(!inside(7, &ranges));
    assert!(inside(8, &ranges));
    assert!(!inside(16, &ranges));
    assert!(inside(39, &ranges));
    assert!(!inside(40, &ranges));
}

#[test]
fn performs_compare_and_swap_with_the_observed_value_on_failure() {
    with_vm(|vm| {
        vm.put(BOOT_HEAP, 41).unwrap();
        vm.set_reg(3, BOOT_HEAP).unwrap();
        vm.set_reg(1, 41).unwrap();
        vm.set_reg(2, 99).unwrap();
        vm.atomic(0xc8e1_fc62).unwrap(); // casal x1, x2, [x3]
        assert_eq!(vm.u64(BOOT_HEAP).unwrap(), 99);
        assert_eq!(vm.reg(1).unwrap(), 41);
        vm.set_reg(1, 42).unwrap();
        vm.set_reg(2, 100).unwrap();
        vm.atomic(0xc8e1_fc62).unwrap();
        assert_eq!(vm.u64(BOOT_HEAP).unwrap(), 99);
        assert_eq!(vm.reg(1).unwrap(), 99);
    });
}

#[test]
fn updates_both_words_of_an_arm64_casp_atomically() {
    with_vm(|vm| {
        vm.put(BOOT_HEAP, 11).unwrap();
        vm.put(BOOT_HEAP + 8, 12).unwrap();
        for (register, value) in [(4, BOOT_HEAP), (0, 11), (1, 12), (2, 21), (3, 22)] {
            vm.set_reg(register, value).unwrap();
        }
        vm.atomic(0x4860_fc82).unwrap(); // caspal x0, x1, x2, x3, [x4]
        assert_eq!(vm.u64(BOOT_HEAP).unwrap(), 21);
        assert_eq!(vm.u64(BOOT_HEAP + 8).unwrap(), 22);
        assert_eq!(vm.reg(0).unwrap(), 11);
        assert_eq!(vm.reg(1).unwrap(), 12);
    });
}

#[test]
fn wraps_a_32_bit_atomic_add_without_overwriting_adjacent_memory() {
    with_vm(|vm| {
        vm.put(BOOT_HEAP, 0xabcd_ef01_ffff_ffff).unwrap();
        vm.set_reg(19, BOOT_HEAP).unwrap();
        vm.set_reg(8, 2).unwrap();
        vm.atomic(0xb8a8_0268).unwrap(); // ldadda w8, w8, [x19]
        assert_eq!(vm.u64(BOOT_HEAP).unwrap(), 0xabcd_ef01_0000_0001);
        assert_eq!(vm.reg(8).unwrap(), 0xffff_ffff);
    });
}

#[test]
fn measures_symbolic_mangled_names_without_treating_payload_zeros_as_terminators() {
    for (data, length) in [
        (&b"SS\0"[..], 2),
        (&[2, 0, 0, 0, 0, b'y', b'S', b'S', b'G', 0][..], 9),
        (&[0x18, 0, 0, 0, 0, 0, 0, 0, 0, b'G', 0][..], 10),
    ] {
        assert_eq!(mangled_name_length(data).unwrap(), length);
    }
}

#[test]
fn rejects_malformed_symbolic_names() {
    for data in [&b"SS"[..], &[1, 0, 0][..], &[0x18, 0, 0, 0, 0][..]] {
        assert!(mangled_name_length(data).is_err());
    }
}

#[test]
fn discovers_relocated_relative_records_and_excludes_invalid_lengths_and_interior_references() {
    for base in [0x2000u64, 0x1_4000_0000] {
        let mut data = Vec::new();
        for word in [0xffff_fff0u32, 9, 0x100, 3, 0xffff_ffe0, 8, 0xffff_ffda, 7] {
            data.extend_from_slice(&word.to_le_bytes());
        }
        let names = HashMap::from([(base - 16, 9), (base + 0x108, 3)]);
        assert_eq!(
            concrete_type_records(base, &data, &names),
            [
                TypeRequest {
                    address: base - 16,
                    length: 9
                },
                TypeRequest {
                    address: base + 0x108,
                    length: 3
                }
            ]
        );
    }
}

#[test]
fn finds_coalesced_got_references_only_when_the_address_register_matches() {
    assert_eq!(
        referenced_pointer(0x9000_0001, 0xf940_2420, 0x2000),
        Some(0x2048)
    );
    assert_eq!(referenced_pointer(0x9000_0002, 0xf940_2420, 0x2000), None);
    assert_eq!(referenced_pointer(0x9000_0001, 0xf900_2420, 0x2000), None);
}

#[test]
fn keeps_generic_witness_instantiations_separate_and_records_only_complete_foreign_metadata() {
    with_vm(|vm| {
        for metadata in [100, 200] {
            vm.set(0, 10).unwrap();
            vm.set(1, metadata).unwrap();
            vm.write_register(RegisterARM64::LR, BOOT_RETURN).unwrap();
            vm.observe_metadata_call("swift_getWitnessTable").unwrap();
            assert_eq!(vm.register(RegisterARM64::LR).unwrap(), BOOT_RETURN);
            vm.set(0, metadata + 1000).unwrap();
            vm.observe_metadata_return(BOOT_RETURN).unwrap();
        }
        let witnesses = &vm.get_data().metadata.swift_witnesses;
        assert_eq!(witnesses[&witness_key(10, 100)], 1100);
        assert_eq!(witnesses[&witness_key(10, 200)], 1200);
        for state in [1, 0] {
            vm.set(1, 42).unwrap();
            vm.observe_metadata_call("swift_getForeignTypeMetadata")
                .unwrap();
            assert_eq!(vm.register(RegisterARM64::LR).unwrap(), BOOT_RETURN);
            vm.set(0, 900).unwrap();
            vm.set(1, state).unwrap();
            vm.observe_metadata_return(BOOT_RETURN).unwrap();
            if state != 0 {
                assert!(vm.get_data().metadata.swift_foreign.is_empty());
            }
        }
        assert_eq!(vm.get_data().metadata.swift_foreign["42"], 900);
    });
}

#[test]
fn uses_the_size_argument_after_alignment_for_zone_allocations_with_options() {
    for name in [
        "malloc_type_zone_malloc_with_options",
        "malloc_zone_malloc_with_options",
    ] {
        with_vm(|vm| {
            vm.get_data_mut().cursor = 16;
            for (register, value) in [(0, 0), (1, 4096), (2, 16384), (3, 2), (4, 2)] {
                vm.set(register, value).unwrap();
            }
            assert!(vm.host(name).unwrap());
            let address = vm.arg(0).unwrap();
            assert_eq!(address % 4096, 0);
            assert_eq!(vm.get_data().allocations[&address], 16384);
            assert!(vm.alloc(16).unwrap() >= address + 16384);
        });
    }
}

/// `State` keeps Go's zero cursor until the first allocation.
#[test]
fn starts_with_an_empty_heap() {
    with_vm(|vm| {
        let state: &State<'_> = vm.get_data();
        assert_eq!(state.cursor, 0);
    });
}
