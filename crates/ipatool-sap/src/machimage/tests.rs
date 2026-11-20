use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::image::{Image, add_signed};
use super::macho::{Bind, Rebase, Segment};
use super::preparation_cache::{
    ChainedRelocation, InstructionPatch, MAX_IMAGE_PREPARATIONS, MAX_PREPARATION_BYTES,
    Preparation, PreparationCache,
};
use super::{Memory, PAGE_SIZE, POINTER_SIZE};
use crate::error::{Error, Result};

const PAGE: usize = 0x1000;

fn segment(name: &str, address: u64, size: u64, file_off: u64, file_size: u64) -> Segment {
    Segment {
        name: name.to_owned(),
        address,
        size,
        file_off,
        file_size,
    }
}

fn image_with(data: Vec<u8>, segments: Vec<Segment>) -> Image<'static> {
    let mut image = Image::empty("test", Cow::Owned(data), None);
    image.segments = segments;
    image
}

fn assert_error<T: std::fmt::Debug>(result: Result<T>, want: &str) {
    let error = result.expect_err("expected an error");
    assert!(
        error.to_string().contains(want),
        "error {error} does not contain {want:?}"
    );
}

#[test]
fn validate_segments() {
    let cases = [
        ("valid", 32, segment("__DATA", 0x1000, 16, 8, 16), ""),
        (
            "file data exceeds memory",
            32,
            segment("__DATA", 0x1000, 8, 8, 16),
            "file data exceeds its memory size",
        ),
        (
            "file data exceeds image",
            16,
            segment("__DATA", 0x1000, 16, 8, 16),
            "data exceeds",
        ),
        (
            "file range overflows",
            32,
            segment("__DATA", 0x1000, 16, u64::MAX - 7, 16),
            "data exceeds",
        ),
        (
            "address range overflows",
            32,
            segment("__DATA", u64::MAX - 7, 16, 0, 0),
            "address range overflows",
        ),
    ];
    for (name, length, item, want) in cases {
        let image = image_with(vec![0; length], vec![item]);
        let result = image.validate_segments();
        if want.is_empty() {
            assert!(result.is_ok(), "{name}: {result:?}");
        } else {
            assert_error(result, want);
        }
    }
}

#[test]
fn segment_file_offset_bounds() {
    let image = image_with(
        vec![0; 64],
        vec![
            segment("__DATA", 0x1000, 24, 16, 16),
            segment("__SHORT", 0x2000, 16, 56, 16),
        ],
    );
    let cases = [
        ("valid at end", "__DATA", 8, Ok(24)),
        ("past file data", "__DATA", 9, Err("exceeds file data")),
        ("past memory", "__DATA", 17, Err("exceeds segment")),
        ("past image", "__SHORT", 8, Err("exceeds test")),
        (
            "offset overflow",
            "__DATA",
            u64::MAX - 3,
            Err("exceeds segment"),
        ),
        ("unknown segment", "__MISSING", 0, Err("unknown segment")),
    ];
    for (name, segment, offset, want) in cases {
        let result = image.segment_file_offset(segment, offset, POINTER_SIZE);
        match want {
            Ok(want) => assert_eq!(result.ok(), Some(want), "{name}"),
            Err(want) => assert_error(result, want),
        }
    }
}

#[test]
fn relocate_rejects_fixups_outside_their_segments() {
    let binds = vec![Bind {
        name: "symbol".to_owned(),
        segment: "__DATA".to_owned(),
        seg_offset: 9,
        ..Bind::default()
    }];
    let rebases = vec![Rebase {
        typ: 1,
        segment: "__DATA".to_owned(),
        offset: 9,
        value: 0x1000,
    }];
    for (binds, rebases) in [(binds, Vec::new()), (Vec::new(), rebases)] {
        let mut image = image_with(vec![0; 64], vec![segment("__DATA", 0x1000, 32, 16, 16)]);
        image.base = 0x1000;
        image.binds = Arc::new(binds);
        image.rebases = Arc::new(rebases);
        assert_error(
            image.relocate(0x2000, &mut |_| Ok(0x3000)),
            "exceeds file data",
        );
    }
}

#[test]
fn add_signed_bounds() {
    assert_eq!(add_signed(0x1000, -4).ok(), Some(0xffc));
    assert_eq!(add_signed(0x1000, i64::MIN).ok(), None);
    assert_eq!(add_signed(u64::MAX, 1).ok(), None);
}

/// One instruction page and one data page following the Mach-O headers.
fn shared_fixture() -> Vec<u8> {
    fn put32(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn put64(data: &mut [u8], offset: usize, value: u64) {
        data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    let length = 3 * PAGE;
    let mut data = vec![0u8; length];
    for (n, value) in [0xfeed_facf, 0x0100_000c, 2, 6, 2, 176, 0, 0]
        .into_iter()
        .enumerate()
    {
        put32(&mut data, n * 4, value);
    }
    put32(&mut data, 32, 25);
    put32(&mut data, 36, 152);
    data[40..46].copy_from_slice(b"__TEXT");
    for (n, value) in [0x1_0000_0000, length as u64, 0, length as u64]
        .into_iter()
        .enumerate()
    {
        put64(&mut data, 56 + n * 8, value);
    }
    for (n, value) in [7, 7, 1, 0].into_iter().enumerate() {
        put32(&mut data, 88 + n * 4, value);
    }
    data[104..110].copy_from_slice(b"__text");
    data[120..126].copy_from_slice(b"__TEXT");
    put64(&mut data, 136, 0x1_0000_1000);
    put64(&mut data, 144, PAGE_SIZE);
    put32(&mut data, 152, 0x1000);
    put32(&mut data, 156, 2);
    put32(&mut data, 168, 0x8000_0400);
    for (n, value) in [2, 24, 512, 1, 528, 8].into_iter().enumerate() {
        put32(&mut data, 184 + n * 4, value);
    }
    put32(&mut data, 512, 1);
    data[516] = 0x0f;
    data[517] = 1;
    put64(&mut data, 520, 0x1_0000_1000);
    data[528..536].copy_from_slice(b"\0target\0");
    put32(&mut data, 4096, 0xd503_233f);
    data
}

struct ImageMemory {
    data: Vec<u8>,
    base: u64,
    fail: bool,
}

impl Memory for ImageMemory {
    fn mem_map(&mut self, _address: u64, _size: u64) -> Result<()> {
        Ok(())
    }

    fn mem_write(&mut self, address: u64, data: &[u8]) -> Result<()> {
        if self.fail {
            return Err(Error::msg("write failure"));
        }
        let start = usize::try_from(address - self.base).expect("guest offset fits");
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }
}

fn target_bind() -> Arc<Vec<Bind>> {
    Arc::new(vec![Bind {
        name: "target".to_owned(),
        typ: 1,
        segment: "__TEXT".to_owned(),
        seg_offset: 8192,
        ..Bind::default()
    }])
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().expect("four bytes"))
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().expect("eight bytes"))
}

#[test]
fn shares_instruction_patches_and_isolates_relocation_and_source_bytes() {
    let source = shared_fixture();
    let original = source.clone();
    let mut first = Image::open_shared("first", &source).expect("open first");
    let mut second = Image::open_shared("second", &source).expect("open second");
    assert!(std::ptr::eq(first.data.as_ptr(), source.as_ptr()));
    assert!(Arc::ptr_eq(&first.instructions, &second.instructions));
    assert!(first.modified.as_ref().is_some_and(HashMap::is_empty));
    assert!(Arc::ptr_eq(
        first.symbols.as_ref().expect("symbols"),
        second.symbols.as_ref().expect("symbols")
    ));
    assert_eq!(
        second.export("target", 0x3_0000_0000).ok(),
        Some(0x3_0000_1000)
    );
    assert_eq!(
        second.instruction_ranges(),
        vec![[0x1_0000_1000, 0x1_0000_2000]]
    );
    assert_eq!(second.symbols().len(), 1);
    first.binds = target_bind();
    second.binds = target_bind();
    first
        .relocate(0x2_0000_0000, &mut |_| Ok(0x1111))
        .expect("relocate first");
    second
        .relocate(0x3_0000_0000, &mut |_| Ok(0x2222))
        .expect("relocate second");
    second
        .put_pointer(4092, 0x8877_6655_4433_2211)
        .expect("put pointer");
    assert_eq!(second.pointer_at(4092), 0x8877_6655_4433_2211);
    assert_eq!(first.pointer_at(4092), 0xd503_201f_0000_0000);
    for (image, base, value) in [
        (&first, 0x2_0000_0000, 0x1111),
        (&second, 0x3_0000_0000, 0x2222),
    ] {
        let mut memory = ImageMemory {
            data: vec![0; source.len()],
            base,
            fail: false,
        };
        image.load(&mut memory).expect("load");
        assert_eq!(read_u64(&memory.data, 8192), value);
        if base == 0x2_0000_0000 {
            assert_eq!(read_u32(&memory.data, 4096), 0xd503_201f);
        }
    }
    assert_eq!(source, original);
}

#[test]
fn keeps_concurrent_guests_independent_while_sharing_preparation() {
    let mut source = shared_fixture();
    let length = source.len();
    source[length - 2] = 77;
    std::thread::scope(|scope| {
        let guests: Vec<_> = (1..=8u64)
            .map(|n| {
                let source = &source;
                scope.spawn(move || {
                    let mut image = Image::open_shared("concurrent", source)?;
                    image.binds = target_bind();
                    let expected = n * 4096;
                    image.relocate(n << 32, &mut |_| Ok(expected))?;
                    if image.pointer_at(8192) == expected {
                        Ok(())
                    } else {
                        Err(Error::msg(format!("guest {n} has another guest's binding")))
                    }
                })
            })
            .collect();
        for guest in guests {
            guest.join().expect("guest thread").expect("guest");
        }
    });
}

#[test]
fn applies_cached_chained_relocations_with_each_guests_own_addresses() {
    let source = vec![0u8; 4096];
    let plan = Arc::new(vec![
        ChainedRelocation {
            offset: 0,
            target: 0x80,
            ..ChainedRelocation::default()
        },
        ChainedRelocation {
            offset: 8,
            is_bind: true,
            symbol: "target".to_owned(),
            addend: -4,
            ..ChainedRelocation::default()
        },
    ]);
    for base in [0x1000u64, 0x2000] {
        let mut image = Image::empty("test", Cow::Borrowed(&source), Some(HashMap::new()));
        image.fixups = Arc::clone(&plan);
        image
            .relocate_chained(base, &mut |_| Ok(base + 0x404))
            .expect("relocate");
        assert_eq!(image.pointer_at(0), base + 0x80);
        assert_eq!(image.pointer_at(8), base + 0x400);
    }
    assert_eq!(source, vec![0; 4096]);
}

#[test]
fn overlays_patches_across_unaligned_page_segment_and_chunk_boundaries() {
    let source = vec![0u8; 3 * PAGE + 13];
    let mut image = Image::empty("test", Cow::Borrowed(&source), Some(HashMap::new()));
    image.write_instruction(4094, 0x1122_3344);
    assert_eq!(image.pointer_at(4092), 0x1122_3344_0000);
    let mut decoded = [0u8; 8];
    assert_eq!(image.read_at(4092, &mut decoded), 8);
    assert_eq!(u64::from_le_bytes(decoded), 0x1122_3344_0000);
    image
        .put_pointer(4095, 0xaabb_ccdd_eeff_0011)
        .expect("put pointer");
    let mut expected = source.clone();
    expected[4094..4098].copy_from_slice(&0x1122_3344u32.to_le_bytes());
    expected[4095..4103].copy_from_slice(&0xaabb_ccdd_eeff_0011u64.to_le_bytes());
    let mut memory = ImageMemory {
        data: vec![0; source.len()],
        base: 0,
        fail: false,
    };
    let mut buffer = vec![0; 4093];
    image
        .write_segment(&mut memory, 0, 3, source.len() as u64, &mut buffer)
        .expect("write segment");
    assert_eq!(memory.data[..source.len() - 3], expected[3..]);
}

#[test]
fn invalidates_preparation_when_any_source_byte_changes() {
    let source = shared_fixture();
    let first = Image::open_shared("first", &source).expect("open first");
    let mut changed = source.clone();
    let last = changed.len() - 1;
    changed[last] = 1;
    let second = Image::open_shared("second", &changed).expect("open second");
    assert!(!Arc::ptr_eq(&first.instructions, &second.instructions));
}

#[test]
fn preserves_write_errors_and_rejects_fixups_outside_the_image() {
    let source = shared_fixture();
    let mut image = Image::open_shared("image", &source).expect("open");
    let base = image.base();
    image
        .relocate(base, &mut |name| {
            Err(Error::msg(format!("unexpected {name}")))
        })
        .expect("relocate");
    let mut memory = ImageMemory {
        data: Vec::new(),
        base: 0,
        fail: true,
    };
    assert_error(image.load(&mut memory), "write failure");
    assert!(image.put_pointer(source.len() as u64 - 7, 1).is_err());
}

#[test]
fn evicts_old_preparations_when_the_image_count_is_exceeded() {
    let cache = PreparationCache::new();
    let key = |n: u8| -> [u8; 32] { Sha256::digest([n]).into() };
    for n in 0..=MAX_IMAGE_PREPARATIONS {
        cache.put(key(u8::try_from(n).expect("small")), Preparation::default());
    }
    assert!(cache.get(&key(0)).is_none());
    assert_eq!(cache.state().entries.len(), MAX_IMAGE_PREPARATIONS);
}

#[test]
fn bounds_retained_patch_and_fixup_storage() {
    let cache = PreparationCache::new();
    let preparation = |capacity: usize| Preparation {
        instructions: Arc::new(HashMap::from([(
            0,
            Vec::<InstructionPatch>::with_capacity(capacity),
        )])),
        ..Preparation::default()
    };
    let first: [u8; 32] = Sha256::digest(b"first").into();
    cache.put(first, preparation(MAX_PREPARATION_BYTES / 16 + 1));
    assert!(cache.get(&first).is_none());
    cache.put(first, preparation(MAX_PREPARATION_BYTES / 32 + 1));
    let second: [u8; 32] = Sha256::digest(b"second").into();
    cache.put(second, preparation(MAX_PREPARATION_BYTES / 32 + 1));
    assert!(cache.get(&first).is_none());
    assert!(cache.state().bytes <= MAX_PREPARATION_BYTES);
}

#[test]
fn removes_pointer_authentication() {
    use super::without_pointer_authentication as strip;
    assert_eq!(strip(0xd71f_0a30), 0xd61f_0220);
    assert_eq!(strip(0xd73f_0811), 0xd63f_0000);
    assert_eq!(strip(0xd65f_0bff), 0xd65f_03c0);
    assert_eq!(strip(0xd503_233f), 0xd503_201f);
    assert_eq!(strip(0xdac1_43e5), 0xd503_201f);
    assert_eq!(strip(0x9100_0000), 0x9100_0000);
}
