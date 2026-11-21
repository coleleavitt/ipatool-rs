//! `appstore_download_parallel_test.go` and `appstore_download_resume_test.go`. The fake
//! transfer executor serves ranges like Go's `http.ServeContent`.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use sha2::{Digest, Sha256};

use super::*;
use crate::download_file::{PARALLEL_THRESHOLD, parts_path};
use crate::{Progress, ProgressHandle};

/// Go's `downloadFixtureData`: byte `i` is `i % 251 + seed`.
struct Fixture {
    seed: u8,
    offset: i64,
    end: i64,
}

impl Read for Fixture {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = usize::try_from((self.end - self.offset).max(0))
            .unwrap()
            .min(buffer.len());
        for (index, byte) in buffer[..count].iter_mut().enumerate() {
            *byte = u8::try_from((self.offset + index as i64) % 251)
                .unwrap()
                .wrapping_add(self.seed);
        }
        self.offset += count as i64;
        Ok(count)
    }
}

fn fixture(seed: u8, start: i64, end: i64) -> Box<dyn Read + Send> {
    Box::new(Fixture {
        seed,
        offset: start,
        end,
    })
}

#[derive(Clone, Debug, PartialEq)]
struct Recorded {
    range: String,
    validator: String,
}

/// Go's `http.ServeContent` for a generated body of `size` bytes.
fn serve(request: &HttpRequest, size: i64, seed: u8, etag: &str) -> HttpResponse {
    let mut headers = Header::new();
    if !etag.is_empty() {
        headers.set("ETag", etag);
    }
    headers.set("Accept-Ranges", "bytes");
    let range = request.headers.get("Range");
    let if_range = request.headers.get("If-Range");
    let honor =
        !range.is_empty() && (if_range.is_empty() || (if_range == etag && !etag.starts_with("W/")));
    let full = |mut headers: Header| HttpResponse {
        status: 200,
        content_length: size,
        body: fixture(seed, 0, size),
        headers: {
            headers.set("Content-Length", size.to_string());
            headers
        },
        request_url: None,
    };
    if !honor {
        return full(headers);
    }
    let spec = range.strip_prefix("bytes=").unwrap();
    let (start, end) = spec.split_once('-').unwrap();
    let start: i64 = start.parse().unwrap();
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<i64>().unwrap().min(size - 1)
    };
    if start >= size {
        headers.set("Content-Range", format!("bytes */{size}"));
        return HttpResponse {
            status: 416,
            headers,
            content_length: 0,
            body: Box::new(io::empty()),
            request_url: None,
        };
    }
    headers.set("Content-Range", format!("bytes {start}-{end}/{size}"));
    HttpResponse {
        status: 206,
        headers,
        content_length: end - start + 1,
        body: fixture(seed, start, end + 1),
        request_url: None,
    }
}

type Handler = Box<dyn Fn(&HttpRequest, &Download) -> HttpResponse + Send + Sync>;

struct Download {
    world: World,
    size: i64,
    path: PathBuf,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Download {
    fn new(size: i64, handler: Option<Handler>) -> Arc<Self> {
        let world = World::new();
        let path = world.dir.path().join("app.ipa.tmp");
        let download = Arc::new(Self {
            world,
            size,
            path,
            requests: Arc::default(),
        });
        let weak = Arc::downgrade(&download);
        let handler = handler.unwrap_or_else(|| {
            Box::new(|request, download| serve(request, download.size, 1, "\"fixture\""))
        });
        download.world.fake.handle_http(move |request| {
            let download = weak.upgrade().expect("fixture alive");
            download.requests.lock().unwrap().push(Recorded {
                range: request.headers.get("Range").to_owned(),
                validator: request.headers.get("If-Range").to_owned(),
            });
            assert_eq!(request.headers.get("Accept-Encoding"), "identity");
            Ok(handler(request, &download))
        });
        download
    }

    fn write_prefix(&self, size: i64) {
        let mut data = Vec::new();
        fixture(1, 0, size).read_to_end(&mut data).unwrap();
        std::fs::write(&self.path, data).unwrap();
    }

    fn run(&self, progress: Option<ProgressHandle>) -> crate::Result<()> {
        self.run_with(progress, true)
    }

    fn run_with(&self, progress: Option<ProgressHandle>, parallel: bool) -> crate::Result<()> {
        self.world.client.download_file_blocking(
            "https://cdn.example/app.ipa",
            &self.path,
            progress.as_ref(),
            parallel,
        )
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn expect_contents(&self, seed: u8) {
        assert_eq!(
            std::fs::metadata(&self.path).unwrap().len(),
            u64::try_from(self.size).unwrap()
        );
        assert_eq!(digest_file(&self.path), digest(fixture(seed, 0, self.size)));
        assert!(!parts_path(&self.path).exists());
    }
}

fn digest(mut reader: impl Read) -> Vec<u8> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = reader.read(&mut buffer).unwrap();
        if read == 0 {
            return hasher.finalize().to_vec();
        }
        hasher.update(&buffer[..read]);
    }
}

fn digest_file(path: &Path) -> Vec<u8> {
    digest(std::fs::File::open(path).unwrap())
}

#[derive(Default)]
struct Counter {
    total: AtomicI64,
    current: AtomicI64,
}

impl Progress for Counter {
    fn phase(&self, _phase: crate::Phase) {}

    fn set_total(&self, total: i64) {
        self.total.store(total, Ordering::SeqCst);
    }

    fn set(&self, position: i64) {
        self.current.store(position, Ordering::SeqCst);
    }

    fn add(&self, bytes: u64) {
        self.current
            .fetch_add(i64::try_from(bytes).unwrap(), Ordering::SeqCst);
    }
}

#[test]
fn uses_four_requests_at_the_threshold_and_counts_progress_once() {
    let prefix = 4096;
    let download = Download::new(PARALLEL_THRESHOLD + prefix, None);
    download.write_prefix(prefix);
    let counter = Arc::new(Counter::default());
    download
        .run(Some(Arc::clone(&counter) as ProgressHandle))
        .unwrap();
    download.expect_contents(1);
    let requests = download.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].range, "bytes=4096-");
    for request in &requests[1..] {
        assert_eq!(request.validator, "\"fixture\"");
        assert!(!request.range.ends_with('-'));
    }
    assert_eq!(counter.current.load(Ordering::SeqCst), download.size);
    assert_eq!(counter.total.load(Ordering::SeqCst), download.size);
}

/// The fork's "uses one request when parallel downloads are disabled".
#[test]
fn no_parallel_uses_one_request() {
    let download = Download::new(PARALLEL_THRESHOLD * 2, None);
    let counter = Arc::new(Counter::default());
    download
        .run_with(Some(Arc::clone(&counter) as ProgressHandle), false)
        .unwrap();
    download.expect_contents(1);
    let requests = download.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].range, "bytes=0-");
    assert_eq!(counter.current.load(Ordering::SeqCst), download.size);
}

/// The fork's `TestDownloadFileReportsCompletedDownload`.
#[test]
fn reports_a_completed_download() {
    let download = Download::new(7, None);
    download.write_prefix(7);
    let counter = Arc::new(Counter::default());
    download
        .run(Some(Arc::clone(&counter) as ProgressHandle))
        .unwrap();
    assert_eq!(download.requests().len(), 1);
    assert_eq!(counter.total.load(Ordering::SeqCst), 7);
    assert_eq!(counter.current.load(Ordering::SeqCst), 7);
}

#[test]
fn uses_one_request_below_the_threshold_or_for_small_files() {
    let prefix = 4097;
    let download = Download::new(PARALLEL_THRESHOLD + prefix - 1, None);
    download.write_prefix(prefix);
    download.run(None).unwrap();
    download.expect_contents(1);
    assert_eq!(download.requests().len(), 1);

    let download = Download::new(64 << 10, None);
    download.run(None).unwrap();
    download.expect_contents(1);
    assert_eq!(download.requests().len(), 1);
}

#[test]
fn keeps_one_stream_when_the_server_ignores_the_range() {
    let download = Download::new(
        PARALLEL_THRESHOLD,
        Some(Box::new(|request, download| {
            let mut request = request.clone();
            request.headers.del("Range");
            serve(&request, download.size, 1, "\"fixture\"")
        })),
    );
    download.write_prefix(4096);
    download.run(None).unwrap();
    download.expect_contents(1);
    assert_eq!(download.requests().len(), 1);
}

#[test]
fn falls_back_to_one_stream_without_appending_invalid_ranges() {
    for (status, content_range, body) in [
        (200, "", "a whole response"),
        (206, "bytes 1-3/134217728", "bad"),
        (206, "bytes 33554432-67108863/134217728", "bad"),
        (503, "", "unavailable"),
    ] {
        let download = Download::new(
            PARALLEL_THRESHOLD,
            Some(Box::new(move |request, download| {
                if request.headers.get("Range").ends_with('-') {
                    return serve(request, download.size, 1, "\"fixture\"");
                }
                let mut response = transfer(status, &[], body.as_bytes().to_vec());
                if !content_range.is_empty() {
                    response.headers.set("Content-Range", content_range);
                }
                response
            })),
        );
        download.run(None).unwrap();
        download.expect_contents(1);
        let requests = download.requests();
        assert!(requests.len() > 1);
        let last = requests.last().unwrap();
        assert!(last.range.ends_with('-'));
        assert_eq!(last.validator, "\"fixture\"");
    }
}

#[test]
fn restarts_the_fallback_when_the_representation_changed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let download = Download::new(
        PARALLEL_THRESHOLD,
        Some(Box::new(move |request, download| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                serve(request, download.size, 1, "\"old\"")
            } else {
                serve(request, download.size, 2, "\"new\"")
            }
        })),
    );
    download.run(None).unwrap();
    download.expect_contents(2);
}

#[test]
fn keeps_a_single_stream_without_a_strong_validator() {
    for validator in ["", "W/\"fixture\""] {
        let download = Download::new(
            PARALLEL_THRESHOLD,
            Some(Box::new(move |request, download| {
                serve(request, download.size, 1, validator)
            })),
        );
        download.run(None).unwrap();
        download.expect_contents(1);
        assert_eq!(download.requests().len(), 1);
    }
}

#[test]
fn falls_back_when_a_range_is_truncated() {
    let download = Download::new(
        PARALLEL_THRESHOLD,
        Some(Box::new(|request, download| {
            let value = request.headers.get("Range");
            if value.ends_with('-') {
                return serve(request, download.size, 1, "\"fixture\"");
            }
            let range = format!("{}/{}", value.replacen('=', " ", 1), download.size);
            let (start, end) = value.trim_start_matches("bytes=").split_once('-').unwrap();
            let length = end.parse::<i64>().unwrap() - start.parse::<i64>().unwrap() + 1;
            HttpResponse {
                status: 206,
                headers: {
                    let mut headers = Header::new();
                    headers.set("Content-Range", range);
                    headers.set("ETag", "\"fixture\"");
                    headers
                },
                content_length: length,
                body: Box::new(Truncated {
                    data: io::Cursor::new(b"truncated".to_vec()),
                }),
                request_url: None,
            }
        })),
    );
    download.run(None).unwrap();
    download.expect_contents(1);
    let requests = download.requests();
    assert!(requests.len() > 1);
    assert!(requests.last().unwrap().range.ends_with('-'));
}

#[test]
fn resumes_the_first_stream_after_a_disconnect() {
    let first = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&first);
    let download = Download::new(
        PARALLEL_THRESHOLD,
        Some(Box::new(move |request, download| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                let size = download.size;
                let mut data = Vec::new();
                fixture(1, 0, 4096).read_to_end(&mut data).unwrap();
                return HttpResponse {
                    status: 206,
                    headers: {
                        let mut headers = Header::new();
                        headers.set("ETag", "\"fixture\"");
                        headers.set("Content-Range", format!("bytes 0-{}/{size}", size - 1));
                        headers
                    },
                    content_length: size,
                    body: Box::new(Truncated {
                        data: io::Cursor::new(data),
                    }),
                    request_url: None,
                };
            }
            serve(request, download.size, 1, "\"fixture\"")
        })),
    );
    let counter = Arc::new(Counter::default());
    download
        .run(Some(Arc::clone(&counter) as ProgressHandle))
        .unwrap();
    download.expect_contents(1);
    let requests = download.requests();
    assert!(requests.len() > 1);
    assert_eq!(requests.last().unwrap().range, "bytes=4096-");
    assert_eq!(counter.current.load(Ordering::SeqCst), download.size);
}

#[test]
fn discards_orphaned_parts_and_resumes_the_main_file() {
    let download = Download::new(64 << 10, None);
    download.write_prefix(4096);
    let directory = parts_path(&download.path);
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("0"), b"out-of-order bytes").unwrap();
    download.run(None).unwrap();
    download.expect_contents(1);
    let requests = download.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].range, "bytes=4096-");
}

#[test]
fn resumes_with_and_without_progress() {
    for with_progress in [false, true] {
        let download = Download::new(10_000, None);
        download.write_prefix(3000);
        let counter = Arc::new(Counter::default());
        let progress = with_progress.then(|| Arc::clone(&counter) as ProgressHandle);
        download.run(progress).unwrap();
        download.expect_contents(1);
        assert_eq!(download.requests()[0].range, "bytes=3000-");
        if with_progress {
            assert_eq!(counter.current.load(Ordering::SeqCst), 10_000);
        }
    }
}

#[test]
fn treats_a_complete_file_as_done() {
    let download = Download::new(10_000, None);
    download.write_prefix(10_000);
    download.run(None).unwrap();
    download.expect_contents(1);
}

#[test]
fn validates_resume_responses() {
    let cases: Vec<(u16, &str, i64, &str)> = vec![
        (
            416,
            "bytes */20000",
            -1,
            "download range rejected: local size 10000 does not match server range \"bytes */20000\"",
        ),
        (
            206,
            "bytes 0-9/20000",
            10,
            "invalid download content range \"bytes 0-9/20000\" for local size 10000",
        ),
        (
            206,
            "bytes 10000-10009/20000",
            5,
            "download content length 5 does not match range length 10",
        ),
        (
            206,
            "bytes 10000-10009",
            10,
            "invalid download content range \"bytes 10000-10009\" for local size 10000",
        ),
        (500, "", 0, "unexpected download response status: 500"),
    ];
    for (status, content_range, length, message) in cases {
        let download = Download::new(
            20_000,
            Some(Box::new(move |_, _| {
                let mut response = transfer(status, &[], Vec::new());
                if !content_range.is_empty() {
                    response.headers.set("Content-Range", content_range);
                }
                response.content_length = length;
                response
            })),
        );
        download.write_prefix(10_000);
        let error = download.run(None).unwrap_err();
        assert_eq!(error.to_string(), message);
        // The prefix is untouched.
        assert_eq!(std::fs::metadata(&download.path).unwrap().len(), 10_000);
    }
}

#[test]
fn reports_a_short_single_stream_as_incomplete() {
    let download = Download::new(
        10_000,
        Some(Box::new(|_, _| {
            let mut response = transfer(200, &[], vec![1; 10]);
            response.content_length = 20;
            response
        })),
    );
    let error = download.run(None).unwrap_err();
    assert_eq!(error.to_string(), "download is incomplete: unexpected EOF");
}
