//! Port of Go `appstore_partial_zip.go`: the display version and release date of one
//! version, read from its IPA's main `Info.plist` through HTTP range requests, so the
//! package is never downloaded whole.

use std::io::{self, Read};
use std::sync::{Mutex, PoisonError};

use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};

use crate::download_types::sprint;
use crate::xar::ReadAt;
use crate::zipfile::{Archive, FileHeader};
use crate::{Client, Error, Header, HttpRequest, Result};

const INFO_PLIST_RELEASE_DATE_KEYS: [&str; 2] = ["releaseDate", "ReleaseDate"];
const INFO_PLIST_DISPLAY_VERSION_KEYS: [&str; 2] =
    ["CFBundleShortVersionString", "bundleShortVersionString"];

/// Go's `versionMetadata`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionMetadata {
    pub display_version: String,
    pub release_date: OffsetDateTime,
}

/// Go's `rangeReaderReadAhead`: the minimum size of a remote read. The zip reader parses
/// the central directory in 4 KiB reads and every request runs through the Store
/// transfer executor, so each miss reads ahead and later reads are served from memory.
pub(crate) const RANGE_READER_READ_AHEAD: u64 = 1 << 20;

/// Go's `httpRangeReaderAt`.
pub(crate) struct HttpRangeReader {
    client: Client,
    url: String,
    size: u64,
    window: Mutex<Window>,
}

#[derive(Default)]
struct Window {
    start: u64,
    data: Vec<u8>,
}

impl HttpRangeReader {
    /// Go's `newHTTPRangeReaderAt`: the reader and the remote size.
    pub(crate) fn new(client: Client, url: &str) -> Result<Self> {
        if url.is_empty() {
            return Err(Error::Invalid("url is empty".to_owned()));
        }
        let size = remote_file_size(&client, url)
            .map_err(|error| error.context("failed to read remote file size"))?;
        Ok(Self::with_size(client, url, size))
    }

    pub(crate) fn with_size(client: Client, url: &str, size: u64) -> Self {
        Self {
            client,
            url: url.to_owned(),
            size,
            window: Mutex::default(),
        }
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    /// Go's `fetch`: bytes `start..=stop` in one range request. A failed body read
    /// returns the bytes received before it.
    fn fetch(&self, start: u64, stop: u64) -> std::result::Result<Vec<u8>, (Vec<u8>, io::Error)> {
        let failed = |error: Error| (Vec::new(), io::Error::other(error.to_string()));
        let mut response = range_request(&self.client, &self.url, &format!("bytes={start}-{stop}"))
            .map_err(failed)?;
        let (actual_start, actual_stop, actual_size) =
            parse_content_range(response.headers.get("Content-Range")).map_err(failed)?;
        if actual_start != start || actual_stop != stop || actual_size != self.size {
            return Err(failed(Error::Invalid(format!(
                "unexpected content range: bytes {actual_start}-{actual_stop}/{actual_size}, expected bytes {start}-{stop}/{}",
                self.size
            ))));
        }
        let length = usize::try_from(stop - start + 1)
            .map_err(|_| failed(Error::Invalid("range is too large".to_owned())))?;
        let mut data = vec![0; length];
        let mut filled = 0;
        while filled < length {
            match response.body.read(&mut data[filled..]) {
                Ok(0) => {
                    let text = if filled == 0 { "EOF" } else { "unexpected EOF" };
                    data.truncate(filled);
                    let error = io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("failed to read response body: {text}"),
                    );
                    return Err((data, error));
                }
                Ok(count) => filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    data.truncate(filled);
                    let error = io::Error::new(
                        error.kind(),
                        format!("failed to read response body: {error}"),
                    );
                    return Err((data, error));
                }
            }
        }
        Ok(data)
    }
}

impl ReadAt for HttpRangeReader {
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        if buffer.is_empty() || offset >= self.size {
            return Ok(0);
        }
        let length = (buffer.len() as u64).min(self.size - offset);
        let end = offset + length;
        let length = usize::try_from(length).unwrap_or(buffer.len());
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        if offset < window.start || end > window.start + window.data.len() as u64 {
            let wanted = (buffer.len() as u64).max(RANGE_READER_READ_AHEAD);
            let fetch_end = offset + wanted.min(self.size - offset);
            match self.fetch(offset, fetch_end - 1) {
                Ok(data) => {
                    *window = Window {
                        start: offset,
                        data,
                    }
                }
                // Only complete responses replace the window. A partial prefix can still
                // serve this read.
                Err((partial, error)) => {
                    if partial.len() < length {
                        return Err(error);
                    }
                    buffer[..length].copy_from_slice(&partial[..length]);
                    return Ok(length);
                }
            }
        }
        let at = usize::try_from(offset - window.start).unwrap_or(usize::MAX);
        buffer[..length].copy_from_slice(&window.data[at..at + length]);
        Ok(length)
    }
}

/// A `GET` with `Range`, through the transfer executor as Go's `httpClient`, that must
/// answer 206.
fn range_request(client: &Client, url: &str, range: &str) -> Result<crate::HttpResponse> {
    let mut headers = Header::new();
    headers.set("Accept-Encoding", "identity");
    headers.set("Range", range);
    let response = client
        .store_http(
            "transfer",
            HttpRequest {
                method: "GET".to_owned(),
                url: url.to_owned(),
                headers,
                body: Vec::new(),
            },
        )
        .map_err(|error| error.context("request failed"))?;
    if response.status != 206 {
        return Err(Error::Invalid(format!(
            "expected partial content response, got status {}",
            response.status
        )));
    }
    Ok(response)
}

/// Go's `remoteFileSize`: a one-byte range probe.
fn remote_file_size(client: &Client, url: &str) -> Result<u64> {
    let response = range_request(client, url, "bytes=0-0")?;
    let (start, end, size) = parse_content_range(response.headers.get("Content-Range"))?;
    if start != 0 || end != 0 {
        return Err(Error::Invalid(format!(
            "unexpected content range for size probe: bytes {start}-{end}/{size}"
        )));
    }
    Ok(size)
}

/// Go's `parseContentRange`: `bytes start-end/size`.
pub(crate) fn parse_content_range(header: &str) -> Result<(u64, u64, u64)> {
    if header.is_empty() {
        return Err(Error::Invalid("content range is empty".to_owned()));
    }
    let invalid = || Error::Invalid(format!("invalid content range: {header}"));
    let (unit_and_range, size) = header.split_once('/').ok_or_else(invalid)?;
    let range = unit_and_range.strip_prefix("bytes ").ok_or_else(invalid)?;
    let (start, end) = range.split_once('-').ok_or_else(invalid)?;
    let (Ok(start), Ok(end), Ok(size)) = (
        start.parse::<i64>(),
        end.parse::<i64>(),
        size.parse::<i64>(),
    ) else {
        return Err(invalid());
    };
    if start < 0 || end < start || end >= size {
        return Err(invalid());
    }
    Ok((
        start.cast_unsigned(),
        end.cast_unsigned(),
        size.cast_unsigned(),
    ))
}

impl Client {
    /// Go's `readVersionMetadataFromIPA`. Blocks: the range reads go through the transfer
    /// executor.
    pub(crate) fn read_version_metadata_from_ipa(&self, url: &str) -> Result<VersionMetadata> {
        let reader = HttpRangeReader::new(self.clone(), url)?;
        let size = reader.size();
        let archive = Archive::new(reader, size)
            .map_err(|error| Error::io("failed to open zip reader", error))?;
        let file = archive
            .files
            .iter()
            .find(|file| is_main_app_info_plist(&file.name))
            .ok_or_else(|| Error::Invalid("could not find Info.plist".to_owned()))?;
        read_version_metadata_from_info_plist(&archive, file)
    }
}

/// Go's `isMainAppInfoPlist`.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "Go's strings.HasSuffix is case-sensitive"
)]
pub(crate) fn is_main_app_info_plist(name: &str) -> bool {
    let parts: Vec<&str> = name.split('/').collect();
    matches!(parts.as_slice(), ["Payload", app, "Info.plist"] if app.ends_with(".app"))
}

/// Go's `readVersionMetadataFromInfoPlist`.
fn read_version_metadata_from_info_plist(
    archive: &Archive<impl ReadAt>,
    file: &FileHeader,
) -> Result<VersionMetadata> {
    let mut source = archive
        .open_file(file)
        .map_err(|error| Error::io("failed to open Info.plist", error))?;
    let mut data = Vec::new();
    source
        .read_to_end(&mut data)
        .map_err(|error| Error::io("failed to read Info.plist", error))?;
    let metadata = plist::Value::from_reader(io::Cursor::new(data))
        .ok()
        .and_then(plist::Value::into_dictionary)
        .ok_or_else(|| {
            Error::Invalid("failed to unmarshal Info.plist: invalid property list".to_owned())
        })?;
    let display_version = read_display_version_from_metadata(&metadata)?;
    let release_date = read_release_date_from_info_plist(&metadata, file.modified())?;
    Ok(VersionMetadata {
        display_version,
        release_date,
    })
}

/// Go's `readDisplayVersionFromMetadata`.
pub(crate) fn read_display_version_from_metadata(metadata: &plist::Dictionary) -> Result<String> {
    for key in INFO_PLIST_DISPLAY_VERSION_KEYS {
        let Some(value) = metadata.get(key) else {
            continue;
        };
        let display_version = sprint(Some(value)).trim().to_owned();
        if display_version.is_empty() || display_version == "<nil>" {
            return Err(Error::Invalid(format!("{key} is empty")));
        }
        return Ok(display_version);
    }
    Err(Error::Invalid(
        "info plist does not contain a display version".to_owned(),
    ))
}

/// Go's `readReleaseDateFromInfoPlist`: a release date key, else the entry's timestamp.
fn read_release_date_from_info_plist(
    metadata: &plist::Dictionary,
    modified: OffsetDateTime,
) -> Result<OffsetDateTime> {
    for key in INFO_PLIST_RELEASE_DATE_KEYS {
        if let Some(value) = metadata.get(key) {
            return parse_release_date_value(value)
                .map_err(|error| error.context("failed to parse release date"));
        }
    }
    // Go's zero-time check cannot fail here: the zip reader always sets a timestamp.
    Ok(modified)
}

/// Go's `parseReleaseDateValue` over howett's decoded types.
fn parse_release_date_value(value: &plist::Value) -> Result<OffsetDateTime> {
    let unix = |seconds: i64| {
        OffsetDateTime::from_unix_timestamp(seconds)
            .map_err(|_| Error::Invalid(format!("timestamp is out of range: {seconds}")))
    };
    match value {
        plist::Value::Date(date) => Ok(OffsetDateTime::from(std::time::SystemTime::from(*date))),
        plist::Value::String(text) => parse_release_date_string(text),
        plist::Value::Integer(number) => match number.as_signed() {
            Some(seconds) => unix(seconds),
            None => Err(Error::Invalid(format!("timestamp is too large: {number}"))),
        },
        // Go's `int64(float64)` truncates toward zero.
        #[allow(clippy::cast_possible_truncation, reason = "Go's conversion")]
        plist::Value::Real(number) => unix(*number as i64),
        other => Err(Error::Invalid(format!(
            "unsupported release date type {}",
            go_type_name(other)
        ))),
    }
}

/// `%T` of the other values howett decodes.
fn go_type_name(value: &plist::Value) -> &'static str {
    match value {
        plist::Value::Boolean(_) => "bool",
        plist::Value::Data(_) => "[]uint8",
        plist::Value::Array(_) => "[]interface {}",
        plist::Value::Dictionary(_) => "map[string]interface {}",
        plist::Value::Uid(_) => "plist.UID",
        _ => "interface {}",
    }
}

/// Go's `parseReleaseDateString`: RFC 3339, `Monday, January 2, 2006` or `2006-01-02`.
pub(crate) fn parse_release_date_string(value: &str) -> Result<OffsetDateTime> {
    let value = value.trim();
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .or_else(|| parse_long_date(value))
        .or_else(|| parse_iso_date(value))
        .map(|date| date.to_offset(time::UtcOffset::UTC))
        .ok_or_else(|| Error::Invalid(format!("invalid release date: {value}")))
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Go's `lookup`: the name that is a case-insensitive prefix of `value`, and the rest.
fn lookup(names: impl IntoIterator<Item = &'static str>, value: &str) -> Option<(usize, &str)> {
    names.into_iter().enumerate().find_map(|(index, name)| {
        let prefix = value.get(..name.len())?;
        prefix
            .eq_ignore_ascii_case(name)
            .then(|| (index, &value[name.len()..]))
    })
}

fn midnight(year: i32, month: u8, day: u8) -> Option<OffsetDateTime> {
    let date = Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()?;
    Some(date.midnight().assume_utc())
}

/// `time.Parse("Monday, January 2, 2006", value)`; the weekday is checked for syntax
/// only, as in Go.
fn parse_long_date(value: &str) -> Option<OffsetDateTime> {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let (_, rest) = lookup(WEEKDAYS, value)?;
    let rest = rest.strip_prefix(", ")?;
    let (month, rest) = lookup(MONTHS, rest)?;
    let rest = rest.strip_prefix(' ')?;
    let digits = rest.bytes().take(2).take_while(u8::is_ascii_digit).count();
    let day: u8 = rest.get(..digits)?.parse().ok()?;
    let year = rest[digits..].strip_prefix(", ")?;
    if year.len() != 4 || !year.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    midnight(year.parse().ok()?, u8::try_from(month + 1).ok()?, day)
}

/// `time.Parse("2006-01-02", value)`.
fn parse_iso_date(value: &str) -> Option<OffsetDateTime> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !value
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return None;
    }
    midnight(
        value[..4].parse().ok()?,
        value[5..7].parse().ok()?,
        value[8..].parse().ok()?,
    )
}
