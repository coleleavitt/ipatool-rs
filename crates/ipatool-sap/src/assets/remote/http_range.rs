//! Port of Go `assets/http_range.go`: random access to the restore image over HTTP ranges.

use std::io::Read;
use std::sync::Mutex;

use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_RANGE, RANGE, USER_AGENT};

use super::ReadAt;
use crate::error::{Context, Error, Result};

const REMOTE_READ_ATTEMPTS: usize = 3;
const RESTORE_IMAGE_USER_AGENT: &str = "Mozilla/5.0";
const REMOTE_SMALL_READ_SIZE: usize = 32 << 10;
const REMOTE_READ_AHEAD_SIZE: usize = 256 << 10;

pub(super) struct RemoteFile {
    client: Client,
    url: String,
    size: u64,
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    cache: Vec<u8>,
    cache_offset: u64,
    previous: bool,
    last_end: u64,
}

impl RemoteFile {
    pub(super) fn open(client: Client, url: &str) -> Result<Self> {
        let mut response = client
            .get(url)
            .header(USER_AGENT, RESTORE_IMAGE_USER_AGENT)
            .header(RANGE, "bytes=0-0")
            .send()
            .context("open remote image")?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(Error::msg(format!(
                "open remote image: server returned {} without byte-range support",
                response.status()
            )));
        }
        let (start, end, size) =
            parse_content_range(content_range(&response)).context("open remote image")?;
        if start != 0 || end != 0 {
            return Err(Error::msg(format!(
                "open remote image: server returned unexpected byte range {start}-{end}"
            )));
        }
        let mut probe = Vec::with_capacity(2);
        (&mut response)
            .take(2)
            .read_to_end(&mut probe)
            .context("read remote image probe")?;
        if probe.len() != 1 {
            return Err(Error::msg("remote image probe has an invalid length"));
        }
        Ok(Self {
            client,
            url: url.to_owned(),
            size,
            state: Mutex::new(CacheState::default()),
        })
    }

    pub(super) fn size(&self) -> u64 {
        self.size
    }

    fn state(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reads and validates one wire range, resuming interrupted bodies.
    fn read_range(&self, data: &mut [u8], offset: u64) -> (usize, Option<Error>) {
        let wanted = data.len();
        let mut total = 0;
        let mut last_error = None;
        for _ in 0..REMOTE_READ_ATTEMPTS {
            if total >= wanted {
                break;
            }
            let start = offset + total as u64;
            let end = offset + wanted as u64 - 1;
            let mut response = match self
                .client
                .get(&self.url)
                .header(USER_AGENT, RESTORE_IMAGE_USER_AGENT)
                .header(RANGE, format!("bytes={start}-{end}"))
                .send()
            {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(Error::Http(error));
                    continue;
                }
            };
            if response.status() != StatusCode::PARTIAL_CONTENT {
                return (
                    total,
                    Some(Error::msg(format!(
                        "read remote image range: server returned {}",
                        response.status()
                    ))),
                );
            }
            match parse_content_range(content_range(&response)) {
                Err(error) => {
                    return (
                        total,
                        Some(Error::msg(format!("read remote image range: {error}"))),
                    );
                }
                Ok((actual_start, actual_end, actual_size))
                    if actual_start != start || actual_end != end || actual_size != self.size =>
                {
                    return (
                        total,
                        Some(Error::msg(format!(
                            "read remote image range: server returned bytes {actual_start}-{actual_end}/{actual_size}, expected {start}-{end}/{}",
                            self.size
                        ))),
                    );
                }
                Ok(_) => {}
            }
            let (count, error) = read_full(&mut response, &mut data[total..wanted]);
            total += count;
            match error {
                None => {
                    last_error = None;
                    break;
                }
                Some(error) => last_error = Some(error),
            }
        }
        if total != wanted {
            let error = last_error.unwrap_or_else(|| Error::msg("unexpected EOF"));
            return (
                total,
                Some(Error::msg(format!(
                    "read remote image at offset {}: {error}",
                    offset + total as u64
                ))),
            );
        }
        (total, None)
    }
}

impl ReadAt for RemoteFile {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if offset >= self.size || data.len() as u64 > self.size - offset {
            return Err(Error::msg("EOF"));
        }
        let wanted = data.len();
        let ahead = {
            let mut state = self.state();
            if offset >= state.cache_offset
                && offset + wanted as u64 <= state.cache_offset + state.cache.len() as u64
            {
                let inside = usize::try_from(offset - state.cache_offset)
                    .map_err(|_| Error::msg("remote image cache offset overflows"))?;
                data.copy_from_slice(&state.cache[inside..inside + wanted]);
                state.previous = true;
                state.last_end = offset + wanted as u64;
                return Ok(());
            }
            // Sparse metadata reads stay exact; only a sequential small-read stream earns
            // read-ahead, so large unused image regions are never fetched.
            let ahead =
                state.previous && offset == state.last_end && wanted <= REMOTE_SMALL_READ_SIZE;
            state.previous = true;
            state.last_end = offset + wanted as u64;
            ahead
        };
        let fetch_size = if ahead {
            usize::try_from((REMOTE_READ_AHEAD_SIZE as u64).min(self.size - offset))
                .unwrap_or(REMOTE_READ_AHEAD_SIZE)
        } else {
            wanted
        };
        let mut buffer = vec![0u8; fetch_size];
        let (count, error) = self.read_range(&mut buffer, offset);
        let copied = count.min(wanted);
        data[..copied].copy_from_slice(&buffer[..copied]);
        if count == fetch_size && error.is_none() && fetch_size <= REMOTE_READ_AHEAD_SIZE {
            let mut state = self.state();
            state.cache_offset = offset;
            state.cache = buffer;
        }
        if count < wanted {
            return Err(error.unwrap_or_else(|| Error::msg("unexpected EOF")));
        }
        Ok(())
    }
}

fn content_range(response: &reqwest::blocking::Response) -> &str {
    response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

fn read_full(reader: &mut impl Read, data: &mut [u8]) -> (usize, Option<Error>) {
    let mut total = 0;
    while total < data.len() {
        match reader.read(&mut data[total..]) {
            Ok(0) => return (total, Some(Error::msg("unexpected EOF"))),
            Ok(count) => total += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return (total, Some(Error::Io(error))),
        }
    }
    (total, None)
}

pub(super) fn parse_content_range(value: &str) -> Result<(u64, u64, u64)> {
    let invalid = || Error::msg(format!("invalid Content-Range {value:?}"));
    let (unit_and_range, total) = value.split_once('/').ok_or_else(invalid)?;
    if total.is_empty() || total == "*" {
        return Err(invalid());
    }
    let size: i64 = total.parse().map_err(|_| invalid())?;
    let size = u64::try_from(size)
        .ok()
        .filter(|size| *size > 0)
        .ok_or_else(invalid)?;
    let range = unit_and_range.strip_prefix("bytes ").ok_or_else(invalid)?;
    let (start, end) = range.split_once('-').ok_or_else(invalid)?;
    let start: i64 = start.parse().map_err(|_| invalid())?;
    let end: i64 = end.parse().map_err(|_| invalid())?;
    let (Ok(start), Ok(end)) = (u64::try_from(start), u64::try_from(end)) else {
        return Err(invalid());
    };
    if end < start || end >= size {
        return Err(invalid());
    }
    Ok((start, end, size))
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    struct Reply {
        status: &'static str,
        content_range: Option<String>,
        body: Vec<u8>,
    }

    struct Server {
        url: String,
        ranges: Arc<Mutex<Vec<String>>>,
    }

    /// Serves each request on its own connection; `None` drops the connection unanswered.
    fn serve(handler: impl Fn(&str, &str) -> Option<Reply> + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/restore.ipsw", listener.local_addr().unwrap());
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let recorded = ranges.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut range, mut agent) = (String::new(), String::new());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("range: ") {
                        range = value.trim().to_owned();
                    } else if lower.starts_with("user-agent: ") {
                        agent = line[12..].trim().to_owned();
                    }
                }
                recorded.lock().unwrap().push(range.clone());
                let Some(reply) = handler(&range, &agent) else {
                    continue;
                };
                let content_range = reply
                    .content_range
                    .map_or_else(String::new, |range| format!("Content-Range: {range}\r\n"));
                let head = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n{content_range}\r\n",
                    reply.status,
                    reply.body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&reply.body);
            }
        });
        Server { url, ranges }
    }

    /// `http.ServeContent` for one byte range.
    fn serve_content(source: Vec<u8>) -> Server {
        serve(move |range, _| Some(range_reply(&source, range)))
    }

    fn range_reply(source: &[u8], range: &str) -> Reply {
        let (start, end) = range
            .strip_prefix("bytes=")
            .and_then(|range| range.split_once('-'))
            .map(|(start, end)| {
                (
                    start.parse::<usize>().unwrap(),
                    end.parse::<usize>().unwrap(),
                )
            })
            .unwrap();
        let end = end.min(source.len() - 1);
        Reply {
            status: "206 Partial Content",
            content_range: Some(format!("bytes {start}-{end}/{}", source.len())),
            body: source[start..=end].to_vec(),
        }
    }

    fn pattern(size: usize) -> Vec<u8> {
        (0..size)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect()
    }

    #[test]
    fn parses_complete_content_range_values() {
        assert_eq!(
            parse_content_range("bytes 0-0/26637307067").unwrap(),
            (0, 0, 26_637_307_067)
        );
        assert_eq!(
            parse_content_range("bytes 100-199/1000").unwrap(),
            (100, 199, 1000)
        );
    }

    #[test]
    fn rejects_incomplete_content_range_values() {
        for value in [
            "bytes 0-0/*",
            "bytes 0-0",
            "bytes 0-0/0",
            "items 0-0/1",
            "bytes 0-1/1",
            "bytes 2-1/3",
        ] {
            assert!(parse_content_range(value).is_err(), "{value}");
        }
    }

    #[test]
    fn reads_ranges_from_a_server_requiring_a_browser_user_agent() {
        const CONTENT: &[u8] = b"restore image";
        let server = serve(|range, agent| {
            Some(if agent == "Mozilla/5.0" {
                range_reply(CONTENT, range)
            } else {
                Reply {
                    status: "403 Forbidden",
                    content_range: None,
                    body: b"unsupported user agent".to_vec(),
                }
            })
        });
        let remote = RemoteFile::open(Client::new(), &server.url).unwrap();
        assert_eq!(remote.size(), CONTENT.len() as u64);
        let mut data = vec![0u8; CONTENT.len() - 1];
        remote.read_at(&mut data, 1).unwrap();
        assert_eq!(data, &CONTENT[1..]);
    }

    #[test]
    fn serves_a_small_sequential_stream_in_bounded_larger_transfers() {
        let source = pattern(1 << 20);
        let server = serve_content(source.clone());
        let remote = RemoteFile::open(Client::new(), &server.url).unwrap();
        let mut output = vec![0u8; source.len()];
        for offset in (0..output.len()).step_by(4096) {
            remote
                .read_at(&mut output[offset..offset + 4096], offset as u64)
                .unwrap();
        }
        assert_eq!(output, source);
        assert_eq!(server.ranges.lock().unwrap().len(), 1 + 5);
        assert!(remote.state().cache.len() <= REMOTE_READ_AHEAD_SIZE);
    }

    #[test]
    fn keeps_sparse_metadata_requests_exact() {
        let source = pattern(1 << 20);
        let server = serve_content(source.clone());
        let remote = RemoteFile::open(Client::new(), &server.url).unwrap();
        for offset in [0usize, 500_000, 900_000] {
            let mut output = [0u8; 8];
            remote.read_at(&mut output, offset as u64).unwrap();
            assert_eq!(output, source[offset..offset + 8]);
        }
        assert_eq!(
            *server.ranges.lock().unwrap(),
            [
                "bytes=0-0",
                "bytes=0-7",
                "bytes=500000-500007",
                "bytes=900000-900007"
            ]
        );
    }

    #[test]
    fn isolates_cached_bytes_from_caller_mutation_and_rejects_reads_past_the_end() {
        let source = pattern(1 << 20);
        let server = serve_content(source.clone());
        let remote = RemoteFile::open(Client::new(), &server.url).unwrap();
        let mut data = vec![0u8; 4096];
        remote.read_at(&mut data, 0).unwrap();
        data.fill(0);
        remote.read_at(&mut data, 0).unwrap();
        assert_eq!(data, source[..4096]);
        let mut tail = [0u8; 32];
        let error = remote
            .read_at(&mut tail, source.len() as u64 - 16)
            .unwrap_err();
        assert_eq!(error.to_string(), "EOF");
    }

    #[test]
    fn supports_concurrent_independent_reads() {
        let source = pattern(1 << 20);
        let server = serve_content(source.clone());
        let remote = RemoteFile::open(Client::new(), &server.url).unwrap();
        std::thread::scope(|scope| {
            for index in 0..8 {
                let (remote, source) = (&remote, &source);
                scope.spawn(move || {
                    let offset = index * 40_000;
                    let mut output = vec![0u8; 4096];
                    remote.read_at(&mut output, offset as u64).unwrap();
                    assert_eq!(output, source[offset..offset + 4096]);
                });
            }
        });
    }

    fn partial(content_range: &str, body: &[u8]) -> Reply {
        Reply {
            status: "206 Partial Content",
            content_range: Some(content_range.to_owned()),
            body: body.to_vec(),
        }
    }

    #[test]
    fn resumes_a_short_response_at_the_first_unread_byte() {
        let calls = Mutex::new(0);
        let server = serve(move |_, _| {
            let mut calls = calls.lock().unwrap();
            *calls += 1;
            match *calls {
                1 => Some(partial("bytes 0-0/8", b"a")),
                2 => Some(partial("bytes 0-7/8", b"abcd")),
                _ => Some(partial("bytes 4-7/8", b"efgh")),
            }
        });
        let remote = RemoteFile::open(Client::new(), &server.url).unwrap();
        let mut data = [0u8; 8];
        remote.read_at(&mut data, 0).unwrap();
        assert_eq!(&data, b"abcdefgh");
        assert_eq!(
            *server.ranges.lock().unwrap(),
            ["bytes=0-0", "bytes=0-7", "bytes=4-7"]
        );
    }

    #[test]
    fn rejects_incorrect_response_ranges_without_caching_them() {
        let calls = Mutex::new(0);
        let server = serve(move |_, _| {
            let mut calls = calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                Some(partial("bytes 0-0/8", b"a"))
            } else {
                Some(partial("bytes 1-7/8", b"1234567"))
            }
        });
        let remote = RemoteFile::open(Client::new(), &server.url).unwrap();
        let error = remote.read_at(&mut [0u8; 8], 0).unwrap_err();
        assert!(
            error.to_string().contains("server returned bytes"),
            "{error}"
        );
        assert_eq!(remote.state().cache.len(), 0);
    }

    #[test]
    fn does_not_fail_a_completed_prefix_when_speculative_tail_reads_fail() {
        let calls = Mutex::new(0);
        let server = serve(move |_, _| {
            let mut calls = calls.lock().unwrap();
            *calls += 1;
            match *calls {
                1 => Some(partial("bytes 0-0/1048576", b"a")),
                2 => Some(partial("bytes 0-3/1048576", b"abcd")),
                3 => Some(partial(
                    &format!("bytes 4-{}/1048576", 4 + REMOTE_READ_AHEAD_SIZE - 1),
                    b"efgh",
                )),
                _ => None,
            }
        });
        let remote = RemoteFile::open(Client::new(), &server.url).unwrap();
        let mut data = [0u8; 4];
        remote.read_at(&mut data, 0).unwrap();
        remote.read_at(&mut data, 4).unwrap();
        assert_eq!(&data, b"efgh");
        let state = remote.state();
        assert_eq!(state.cache_offset, 0);
        assert_eq!(state.cache.len(), 4);
    }

    #[test]
    fn requires_exactly_the_requested_probe_byte() {
        for (body, valid) in [(&b"x"[..], true), (b"", false), (b"xx", false)] {
            let body = body.to_vec();
            let server = serve(move |_, _| Some(partial("bytes 0-0/123", &body)));
            match RemoteFile::open(Client::new(), &server.url) {
                Ok(remote) => {
                    assert!(valid);
                    assert_eq!(remote.size(), 123);
                }
                Err(error) => {
                    assert!(!valid);
                    assert!(
                        error.to_string().contains("probe has an invalid length"),
                        "{error}"
                    );
                }
            }
        }
    }
}
