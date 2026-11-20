//! `appstore_partial_zip_test.go` and the `HTTPRangeReaderAt` cases of
//! `appstore_get_version_metadata_test.go`: range requests answered by the fake transfer
//! executor, as Go's `httptest` server does.

use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::Compression;
use flate2::write::DeflateEncoder;
use plist::Value as PValue;
use time::OffsetDateTime;

use super::*;
use crate::partial_zip::{HttpRangeReader, RANGE_READER_READ_AHEAD};
use crate::xar::ReadAt;
use crate::zipfile::{DEFLATE, FileHeader, STORE, Writer};

const READ_AHEAD: usize = 1 << 20;
const URL: &str = "https://example.test/app.ipa";

/// What the fake server saw.
#[derive(Default)]
pub(crate) struct Served {
    pub ranges: Mutex<Vec<String>>,
    pub bytes: AtomicU64,
    pub whole: AtomicU64,
}

impl Served {
    pub fn ranges(&self) -> Vec<String> {
        self.ranges.lock().unwrap().clone()
    }
}

/// Go's `testRangeBounds`.
pub(crate) fn range_bounds(header: &str, size: usize) -> Option<(usize, usize)> {
    let (start, end) = header.strip_prefix("bytes=")?.split_once('-')?;
    let start: usize = start.parse().ok()?;
    let end = if end.is_empty() {
        size.checked_sub(1)?
    } else {
        end.parse().ok()?
    };
    (start < size && end >= start).then(|| (start, end.min(size - 1)))
}

/// Go's `testIPAServerWithRangeLog`.
pub(crate) fn serve(world: &World, data: Vec<u8>) -> Arc<Served> {
    let served = Arc::new(Served::default());
    let log = Arc::clone(&served);
    world.fake.handle_http(move |request| {
        if request.method != "GET" {
            return Ok(transfer(405, &[], Vec::new()));
        }
        let range = request.headers.get("Range").to_owned();
        log.ranges.lock().unwrap().push(range.clone());
        if range.is_empty() {
            log.whole.fetch_add(1, Ordering::SeqCst);
            return Ok(transfer(200, &[], data.clone()));
        }
        let Some((start, end)) = range_bounds(&range, data.len()) else {
            return Ok(transfer(416, &[], Vec::new()));
        };
        log.bytes
            .fetch_add((end - start + 1) as u64, Ordering::SeqCst);
        Ok(transfer(
            206,
            &[
                ("Accept-Ranges", "bytes".to_owned()),
                (
                    "Content-Range",
                    format!("bytes {start}-{end}/{}", data.len()),
                ),
            ],
            data[start..=end].to_vec(),
        ))
    });
    served
}

/// Go's `testRangeResponseReader`: a reader of `size` bytes whose range requests the
/// handler answers with (status 206, `Content-Range`, body).
fn scripted(
    world: &World,
    size: u64,
    respond: impl Fn(&str) -> (String, Vec<u8>) + Send + Sync + 'static,
) -> (HttpRangeReader, Arc<AtomicU64>) {
    let calls = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&calls);
    world.fake.handle_http(move |request| {
        counter.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.url, URL);
        assert_eq!(request.headers.get("Accept-Encoding"), "identity");
        let (range, body) = respond(request.headers.get("Range"));
        Ok(transfer(206, &[("Content-Range", range)], body))
    });
    (
        HttpRangeReader::with_size(world.client.clone(), URL, size),
        calls,
    )
}

fn open(world: &World, url: &str) -> HttpRangeReader {
    HttpRangeReader::new(world.client.clone(), url).unwrap()
}

fn pattern() -> Vec<u8> {
    (0..2 * READ_AHEAD)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect()
}

/// Go's zip writer's MS-DOS date and time.
fn dos_time(time: OffsetDateTime) -> (u16, u16) {
    let date = (u16::try_from(time.year() - 1980).unwrap() << 9)
        | (u16::from(u8::from(time.month())) << 5)
        | u16::from(time.day());
    let clock = (u16::from(time.hour()) << 11)
        | (u16::from(time.minute()) << 5)
        | u16::from(time.second() / 2);
    (date, clock)
}

/// Go's `testIPA`: a 1 MiB stored filler, then a deflated `Info.plist` whose entry carries
/// `modified` as MS-DOS time and an extended timestamp, as Go's writer stores it.
pub(crate) fn test_ipa(
    display_version: &str,
    release_date: Option<PValue>,
    modified: OffsetDateTime,
) -> Vec<u8> {
    let mut writer = Writer::new(Vec::new());
    let filler = vec![0; READ_AHEAD];
    writer
        .create_raw(
            FileHeader {
                name: "Payload/Test.app/Filler.bin".to_owned(),
                method: STORE,
                creator_version: 20,
                reader_version: 20,
                crc32: crc32fast::hash(&filler),
                compressed_size64: filler.len() as u64,
                uncompressed_size64: filler.len() as u64,
                ..FileHeader::default()
            },
            Vec::new(),
        )
        .unwrap();
    writer.write_all(&filler).unwrap();

    let mut info = vec![
        ("CFBundleExecutable", s("Test")),
        ("CFBundleShortVersionString", s(display_version)),
    ];
    if let Some(date) = release_date {
        info.push(("releaseDate", date));
    }
    let mut plist_data = Vec::new();
    plist::to_writer_binary(&mut plist_data, &PValue::Dictionary(dict(&info))).unwrap();
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&plist_data).unwrap();
    let compressed = encoder.finish().unwrap();
    let mut timestamp = vec![0x55, 0x54, 5, 0, 1];
    timestamp.extend_from_slice(
        &u32::try_from(modified.unix_timestamp())
            .unwrap()
            .to_le_bytes(),
    );
    let (modified_date, modified_time) = dos_time(modified);
    writer
        .create_raw(
            FileHeader {
                name: "Payload/Test.app/Info.plist".to_owned(),
                method: DEFLATE,
                creator_version: 20,
                reader_version: 20,
                modified_date,
                modified_time,
                crc32: crc32fast::hash(&plist_data),
                compressed_size64: compressed.len() as u64,
                uncompressed_size64: plist_data.len() as u64,
                extra: timestamp.clone(),
                ..FileHeader::default()
            },
            timestamp,
        )
        .unwrap();
    writer.write_all(&compressed).unwrap();
    writer.finish().unwrap()
}

#[test]
fn clamps_reads_that_cross_eof() {
    let world = World::new();
    let served = serve(&world, b"abcdef".to_vec());
    let reader = open(&world, URL);
    assert_eq!(reader.size(), 6);
    let mut buffer = [0; 4];
    assert_eq!(reader.read_at(&mut buffer, 4).unwrap(), 2);
    assert_eq!(&buffer[..2], b"ef");
    assert!(served.ranges().contains(&"bytes=4-5".to_owned()));
}

#[test]
fn serves_small_sequential_reads_from_one_read_ahead_request() {
    let world = World::new();
    let data = b"0123456789abcdef".repeat(4096);
    let served = serve(&world, data.clone());
    let reader = open(&world, URL);
    let mut buffer = [0; 4096];
    let mut offset = 1024;
    while offset + buffer.len() <= data.len() {
        assert_eq!(reader.read_at(&mut buffer, offset as u64).unwrap(), 4096);
        assert_eq!(buffer[..], data[offset..offset + 4096]);
        offset += buffer.len();
    }
    // The first entry is the size probe made when the reader is opened.
    assert_eq!(
        served.ranges(),
        [
            "bytes=0-0".to_owned(),
            format!("bytes=1024-{}", data.len() - 1)
        ]
    );
}

#[test]
fn refills_a_read_that_crosses_the_cached_window() {
    let world = World::new();
    let data = pattern();
    let served = serve(&world, data.clone());
    let reader = open(&world, URL);
    let mut buffer = [0; 4096];
    for offset in [0, READ_AHEAD - 2048, READ_AHEAD] {
        assert_eq!(reader.read_at(&mut buffer, offset as u64).unwrap(), 4096);
        assert_eq!(buffer[..], data[offset..offset + 4096]);
    }
    assert_eq!(
        served.ranges(),
        [
            "bytes=0-0".to_owned(),
            format!("bytes=0-{}", READ_AHEAD - 1),
            format!("bytes={}-{}", READ_AHEAD - 2048, 2 * READ_AHEAD - 2049),
        ]
    );
}

#[test]
fn refills_backward_reads_and_isolates_cached_data_from_caller_mutation() {
    let world = World::new();
    let data = pattern();
    let served = serve(&world, data.clone());
    let reader = open(&world, URL);
    let mut buffer = [0; 4096];
    for offset in [READ_AHEAD, 0, 0] {
        buffer.fill(0);
        assert_eq!(reader.read_at(&mut buffer, offset as u64).unwrap(), 4096);
        assert_eq!(buffer[..], data[offset..offset + 4096]);
    }
    assert_eq!(served.ranges().len(), 3);
}

#[test]
fn preserves_eof_behavior_when_reads_are_served_from_the_cache() {
    let world = World::new();
    let served = serve(&world, b"abcdef".to_vec());
    let reader = open(&world, URL);
    assert_eq!(reader.read_at(&mut [0; 1], 0).unwrap(), 1);
    let mut buffer = [0; 4];
    // A short read is Go's `io.EOF`.
    assert_eq!(reader.read_at(&mut buffer, 4).unwrap(), 2);
    assert_eq!(&buffer[..2], b"ef");
    assert_eq!(reader.read_at(&mut buffer[..2], 4).unwrap(), 2);
    assert_eq!(reader.read_at(&mut buffer, reader.size()).unwrap(), 0);
    assert_eq!(served.ranges(), ["bytes=0-0", "bytes=0-5"]);
}

#[test]
fn supports_independent_concurrent_reads() {
    let world = World::new();
    let data = pattern();
    serve(&world, data.clone());
    let reader = open(&world, URL);
    std::thread::scope(|scope| {
        let reads: Vec<_> = (0..8)
            .map(|index| {
                let (reader, data) = (&reader, &data);
                scope.spawn(move || {
                    let offset = index * 200_000;
                    let mut buffer = [0; 4096];
                    reader.read_at(&mut buffer, offset as u64).unwrap();
                    buffer[..] == data[offset..offset + 4096]
                })
            })
            .collect();
        for read in reads {
            assert!(read.join().unwrap(), "incorrect concurrent range");
        }
    });
}

#[test]
fn preserves_received_bytes_when_read_ahead_is_interrupted() {
    for (body, error) in [
        ("abcd", None),
        ("abcdef", None),
        ("abc", Some("failed to read response body: unexpected EOF")),
        ("", Some("failed to read response body: EOF")),
    ] {
        let world = World::new();
        let size = RANGE_READER_READ_AHEAD;
        let (reader, calls) = scripted(&world, size, move |_| {
            (
                format!("bytes 0-{}/{size}", size - 1),
                body.as_bytes().to_vec(),
            )
        });
        for _ in 0..2 {
            let mut buffer = [0; 4];
            match (reader.read_at(&mut buffer, 0), error) {
                (Ok(count), None) => {
                    assert_eq!(count, 4);
                    assert_eq!(&buffer, &body.as_bytes()[..4]);
                }
                (Err(got), Some(expected)) => {
                    assert_eq!(got.kind(), std::io::ErrorKind::UnexpectedEof);
                    assert_eq!(got.to_string(), expected);
                }
                (result, _) => panic!("{body:?}: {result:?}"),
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn retains_the_previous_cache_after_an_interrupted_read_ahead_request() {
    let world = World::new();
    let size = 2 * RANGE_READER_READ_AHEAD;
    let (reader, calls) = scripted(&world, size, move |range| {
        let (start, stop) = range_bounds(range, usize::try_from(size).unwrap()).unwrap();
        let body = if start == 0 {
            vec![b'a'; READ_AHEAD]
        } else {
            b"bcde".to_vec()
        };
        (format!("bytes {start}-{stop}/{size}"), body)
    });
    let mut buffer = [0; 4];
    for offset in [0, RANGE_READER_READ_AHEAD, 0] {
        reader.read_at(&mut buffer, offset).unwrap();
        let expected: &[u8] = if offset == 0 { b"aaaa" } else { b"bcde" };
        assert_eq!(buffer, expected);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn rejects_invalid_response_ranges_before_caching() {
    for header in [
        "",
        "bytes 5-31/32",
        "bytes 4-30/32",
        "bytes 4-31/33",
        "bytes 4-31/*",
        "items 4-31/32",
        "bytes -4-31/32",
        "bytes 31-4/32",
        "bytes 4-32/32",
    ] {
        let world = World::new();
        let current = Arc::new(Mutex::new(header.to_owned()));
        let answer = Arc::clone(&current);
        let (reader, calls) = scripted(&world, 32, move |_| {
            (answer.lock().unwrap().clone(), b"x".repeat(28))
        });
        let mut buffer = [0; 4];
        let error = reader.read_at(&mut buffer, 4).unwrap_err();
        assert!(
            error.to_string().contains("content range"),
            "{header}: {error}"
        );
        "bytes 4-31/32".clone_into(&mut current.lock().unwrap());
        assert_eq!(reader.read_at(&mut buffer, 4).unwrap(), 4);
        assert_eq!(&buffer, b"xxxx");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn clamps_eof_reads_without_overflowing_large_offsets() {
    let world = World::new();
    let size = i64::MAX.cast_unsigned();
    let (reader, _) = scripted(&world, size, move |range| {
        assert_eq!(range, format!("bytes={}-{}", size - 4, size - 1));
        (
            format!("bytes {}-{}/{size}", size - 4, size - 1),
            b"tail".to_vec(),
        )
    });
    let mut buffer = [0; 8];
    assert_eq!(reader.read_at(&mut buffer, size - 4).unwrap(), 4);
    assert_eq!(&buffer[..4], b"tail");
}

#[test]
fn reports_go_errors_for_the_size_probe() {
    let world = World::new();
    world
        .fake
        .handle_http(|_| Ok(transfer(200, &[], b"whole".to_vec())));
    let error = HttpRangeReader::new(world.client.clone(), URL)
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "failed to read remote file size: expected partial content response, got status 200"
    );
    let error = HttpRangeReader::new(world.client.clone(), "")
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "url is empty");
    world.fake.handle_http(|_| {
        Ok(transfer(
            206,
            &[("Content-Range", "bytes 0-1/9".to_owned())],
            b"ab".to_vec(),
        ))
    });
    let error = HttpRangeReader::new(world.client.clone(), URL)
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "failed to read remote file size: unexpected content range for size probe: bytes 0-1/9"
    );
}

#[test]
fn reads_go_modification_times_from_extra_fields() {
    let header = |extra: Vec<u8>, date: u16, time: u16| FileHeader {
        extra,
        modified_date: date,
        modified_time: time,
        ..FileHeader::default()
    };
    // MS-DOS 2024-02-30 25:61:62 normalizes as Go's time.Date.
    let dos = (44 << 9) | (2 << 5) | 0x1e;
    assert_eq!(
        header(Vec::new(), dos, (25 << 11) | (61 << 5) | 31).modified(),
        time::macros::datetime!(2024-03-02 02:02:02 UTC)
    );
    // NTFS: 100 ns ticks since 1601 in attribute 1.
    let ticks: u64 = (11_644_473_600 + 1_700_000_000) * 10_000_000 + 5;
    let mut ntfs = vec![0x0a, 0, 32, 0, 0, 0, 0, 0, 1, 0, 24, 0];
    ntfs.extend_from_slice(&ticks.to_le_bytes());
    ntfs.extend_from_slice(&[0; 16]);
    assert_eq!(
        header(ntfs, dos, 0).modified(),
        OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_000_000_500).unwrap()
    );
    // Info-ZIP Unix (access time, then modification time); the last field wins.
    let mut unix = vec![0x55, 0x58, 8, 0, 9, 9, 9, 9];
    unix.extend_from_slice(&1_600_000_000_u32.to_le_bytes());
    unix.extend_from_slice(&[0x55, 0x54, 5, 0, 0, 1, 2, 3, 4]);
    assert_eq!(
        header(unix, dos, 0).modified(),
        OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap()
    );
}
