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
