//! Cookie jar that reads and writes the same `cookies` file as the Go ipatool
//! (`github.com/juju/persistent-cookiejar`), so a session started by either tool
//! carries over. Only persistent cookies (with `Expires` or `Max-Age`) are saved.
//!
//! Saving locks `<file>.lock` like Go's `go4/lock` (an `fcntl` write lock where Go uses
//! one, else its portable owner-PID file), merges the file's
//! entries (the newer `Updated` wins), drops entries expired for over a day, and
//! rewrites the file.

use std::collections::HashMap;
use std::fs;
use std::io::{ErrorKind, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant};

use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, macros::datetime};
use url::Url;

/// Go's `endOfTime`, the expiry of session cookies.
const END_OF_TIME: OffsetDateTime = datetime!(9999-12-31 23:59:59 UTC);
const EXPIRY_REMOVAL: Duration = Duration::hours(24);
const LOCK_TIMEOUT: StdDuration = StdDuration::from_secs(3);

/// One cookie in Go's on-disk layout.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools, reason = "mirrors the Go file format")]
struct Entry {
    name: String,
    value: String,
    domain: String,
    path: String,
    secure: bool,
    http_only: bool,
    persistent: bool,
    host_only: bool,
    #[serde(with = "time::serde::rfc3339")]
    expires: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    creation: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    last_access: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated: OffsetDateTime,
    canonical_host: String,
}

impl Entry {
    fn id(&self) -> String {
        format!("{};{};{}", self.domain, self.path, self.name)
    }

    fn domain_matches(&self, host: &str) -> bool {
        self.domain == host || (!self.host_only && has_dot_suffix(host, &self.domain))
    }

    fn path_matches(&self, request: &str) -> bool {
        request == self.path
            || (request.starts_with(&self.path)
                && (self.path.ends_with('/') || request.as_bytes()[self.path.len()] == b'/'))
    }

    fn should_send(&self, https: bool, host: &str, path: &str) -> bool {
        self.domain_matches(host) && self.path_matches(path) && (https || !self.secure)
    }
}

/// A Go-compatible persistent cookie jar; plug it into `reqwest` with
/// `cookie_provider`.
#[derive(Debug)]
pub struct CookieJar {
    path: PathBuf,
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug, thiserror::Error)]
pub enum CookieJarError {
    #[error("cannot acquire lock: {0}")]
    Lock(std::io::Error),
    #[error("file locked for too long; giving up")]
    LockTimeout,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("cannot load cookies: {0}")]
    Json(#[from] serde_json::Error),
}

impl CookieJar {
    /// Loads `path` when it exists. Like Go, a missing parent directory gives an
    /// empty jar and invalid JSON fails, while JSON of another shape is discarded.
    ///
    /// # Errors
    /// The lock cannot be taken, or the file cannot be read or parsed.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, CookieJarError> {
        let jar = Self {
            path: path.into(),
            entries: Mutex::new(HashMap::new()),
        };
        if jar.path.parent().is_some_and(Path::exists) {
            let _lock = FileLock::acquire(&lock_path(&jar.path))?;
            match fs::read(&jar.path) {
                Ok(data) => jar.merge(&data)?,
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        jar.delete_expired(OffsetDateTime::now_utc());
        Ok(jar)
    }

    /// Writes persistent cookies back, merged with whatever the file holds now.
    ///
    /// # Errors
    /// The lock cannot be taken or the file cannot be written.
    pub fn save(&self) -> Result<(), CookieJarError> {
        let _lock = FileLock::acquire(&lock_path(&self.path))?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&self.path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        // Go logs and ignores a corrupt file here.
        let _ignored = self.merge(&data);
        self.delete_expired(OffsetDateTime::now_utc());

        let mut entries: Vec<Entry> = self
            .lock()
            .values()
            .filter(|entry| entry.persistent)
            .cloned()
            .collect();
        entries.sort_by(|a, b| {
            a.canonical_host
                .cmp(&b.canonical_host)
                .then_with(|| by_path_length(a, b))
        });
        let mut json = serde_json::to_vec(&entries)?;
        json.push(b'\n');
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(&json)?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn merge(&self, data: &[u8]) -> Result<(), serde_json::Error> {
        if data.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let raw: serde_json::Value = serde_json::from_slice(data)?;
        let Ok(stored) = serde_json::from_value::<Vec<Entry>>(raw) else {
            return Ok(());
        };
        let mut entries = self.lock();
        for entry in stored {
            if entry.canonical_host.is_empty() {
                continue;
            }
            let id = entry.id();
            if entries
                .get(&id)
                .is_none_or(|old| entry.updated > old.updated)
            {
                entries.insert(id, entry);
            }
        }
        Ok(())
    }

    fn delete_expired(&self, now: OffsetDateTime) {
        self.lock()
            .retain(|_, entry| entry.expires > now || entry.updated + EXPIRY_REMOVAL > now);
    }

    fn header_for(&self, url: &Url, now: OffsetDateTime) -> Option<String> {
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let host = canonical_host(url)?;
        let https = url.scheme() == "https";
        let path = if url.path().is_empty() {
            "/"
        } else {
            url.path()
        };

        let mut entries = self.lock();
        let mut selected = Vec::new();
        for entry in entries.values_mut() {
            if entry.expires <= now {
                entry.value.clear();
                continue;
            }
            if entry.should_send(https, &host, path) {
                entry.last_access = now;
                selected.push(entry.clone());
            }
        }
        if selected.is_empty() {
            return None;
        }
        selected.sort_by(by_path_length);
        Some(
            selected
                .iter()
                .map(|entry| format!("{}={}", entry.name, entry.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    fn store(&self, url: &Url, set_cookies: &[&str], now: OffsetDateTime) {
        if !matches!(url.scheme(), "http" | "https") {
            return;
        }
        let Some(host) = canonical_host(url) else {
            return;
        };
        let default_path = default_path(url.path());
        let mut entries = self.lock();
        for header in set_cookies {
            let Ok(cookie) = cookie::Cookie::parse(*header) else {
                continue;
            };
            let Some(mut entry) = new_entry(&cookie, now, &default_path, &host) else {
                continue;
            };
            let id = entry.id();
            entry.creation = entries.get(&id).map_or(now, |old| old.creation);
            entries.insert(id, entry);
        }
    }
}

impl reqwest::cookie::CookieStore for CookieJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        let headers: Vec<&str> = cookie_headers
            .filter_map(|value| value.to_str().ok())
            .collect();
        self.store(url, &headers, OffsetDateTime::now_utc());
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        self.header_for(url, OffsetDateTime::now_utc())
            .and_then(|header| HeaderValue::from_str(&header).ok())
    }
}

/// Go's `newEntry` and `domainAndType`. `None` drops the cookie.
fn new_entry(
    cookie: &cookie::Cookie<'_>,
    now: OffsetDateTime,
    default_path: &str,
    host: &str,
) -> Option<Entry> {
    let path = match cookie.path() {
        Some(path) if path.starts_with('/') => path.to_owned(),
        _ => default_path.to_owned(),
    };
    let (domain, host_only) = domain_and_type(host, cookie.domain_raw())?;
    let mut entry = Entry {
        name: cookie.name().to_owned(),
        value: String::new(),
        domain,
        path,
        secure: false,
        http_only: false,
        persistent: false,
        host_only,
        expires: END_OF_TIME,
        creation: now,
        last_access: now,
        updated: now,
        canonical_host: host.to_owned(),
    };
    if let Some(max_age) = cookie.max_age() {
        entry.persistent = true;
        if max_age <= Duration::ZERO {
            // Go turns `Max-Age<=0` into -1: expire at once.
            entry.expires = now - Duration::seconds(1);
            return Some(entry);
        }
        entry.expires = now + max_age;
    } else if let Some(expires) = cookie.expires_datetime() {
        entry.persistent = true;
        entry.expires = expires;
        if expires <= now {
            return Some(entry);
        }
    }
    cookie.value().clone_into(&mut entry.value);
    entry.secure = cookie.secure().unwrap_or(false);
    entry.http_only = cookie.http_only().unwrap_or(false);
    Some(entry)
}

fn domain_and_type(host: &str, domain: Option<&str>) -> Option<(String, bool)> {
    let Some(domain) = domain.filter(|domain| !domain.is_empty()) else {
        return Some((host.to_owned(), true));
    };
    if host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let domain = domain.strip_prefix('.').unwrap_or(domain);
    if domain.is_empty() || domain.starts_with('.') || domain.ends_with('.') {
        return None;
    }
    let domain = domain.to_ascii_lowercase();
    if psl::suffix_str(&domain) == Some(domain.as_str()) {
        return (host == domain).then(|| (host.to_owned(), true));
    }
    (host == domain || has_dot_suffix(host, &domain)).then_some((domain, false))
}

fn canonical_host(url: &Url) -> Option<String> {
    let host = url
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']');
    Some(host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase())
}

fn default_path(path: &str) -> String {
    if !path.starts_with('/') {
        return "/".to_owned();
    }
    match path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(index) => path[..index].to_owned(),
    }
}

fn has_dot_suffix(value: &str, suffix: &str) -> bool {
    value.len() > suffix.len()
        && value.ends_with(suffix)
        && value.as_bytes()[value.len() - suffix.len() - 1] == b'.'
}

/// Go's `byPathLength`: longer paths first, then older cookies.
fn by_path_length(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    b.path
        .len()
        .cmp(&a.path.len())
        .then(a.creation.cmp(&b.creation))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.value.cmp(&b.value))
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    name.into()
}

/// The `go4.org/lock` lock Go's cookie jar takes on `<file>.lock`, released (and the
/// file removed) on drop.
struct FileLock {
    path: PathBuf,
    file: Option<fs::File>,
}

impl FileLock {
    fn acquire(path: &Path) -> Result<Self, CookieJarError> {
        let started = Instant::now();
        let mut delay = StdDuration::from_micros(100);
        loop {
            match go4_lock::try_lock(path) {
                Ok(file) => {
                    return Ok(Self {
                        path: path.to_owned(),
                        file: Some(file),
                    });
                }
                Err(err) if started.elapsed() >= LOCK_TIMEOUT => {
                    return Err(if err.kind() == ErrorKind::WouldBlock {
                        CookieJarError::LockTimeout
                    } else {
                        CookieJarError::Lock(err)
                    });
                }
                Err(_) => {
                    std::thread::sleep(delay);
                    delay = (delay * 3 / 2).min(StdDuration::from_millis(100));
                }
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Go removes an fcntl lock file before closing it, and closes a portable one
        // first because Windows cannot remove an open file. A failed removal leaves a
        // file the next lock reuses (fcntl) or finds stale (portable).
        if go4_lock::CLOSE_BEFORE_REMOVE {
            drop(self.file.take());
        }
        let _ignored = fs::remove_file(&self.path);
    }
}

// `go4/lock` uses fcntl only in `lock_linux_amd64.go`, `lock_linux_arm.go`,
// `lock_darwin_amd64.go` and `lock_freebsd.go`, and `lockPortable` everywhere else
// (Windows, Apple silicon, arm64 Linux).
#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "arm")),
    all(target_os = "macos", target_arch = "x86_64"),
    target_os = "freebsd"
))]
use fcntl_lock as go4_lock;
#[cfg(not(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "arm")),
    all(target_os = "macos", target_arch = "x86_64"),
    target_os = "freebsd"
)))]
use portable_lock as go4_lock;

#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "arm")),
    all(target_os = "macos", target_arch = "x86_64"),
    target_os = "freebsd"
))]
mod fcntl_lock {
    use std::fs;
    use std::path::Path;

    pub(super) const CLOSE_BEFORE_REMOVE: bool = false;

    /// `lockFcntl`: a zero-size file, write-locked.
    pub(super) fn try_lock(path: &Path) -> std::io::Result<fs::File> {
        if fs::metadata(path).is_ok_and(|meta| meta.len() > 0) {
            return Err(std::io::Error::other(format!(
                "can't Lock file {}: has non-zero size",
                path.display()
            )));
        }
        let file = fs::File::create(path)?;
        rustix::fs::fcntl_lock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
        Ok(file)
    }
}

#[cfg(any(
    test,
    not(any(
        all(target_os = "linux", any(target_arch = "x86_64", target_arch = "arm")),
        all(target_os = "macos", target_arch = "x86_64"),
        target_os = "freebsd"
    ))
))]
mod portable_lock {
    use std::fs;
    use std::io::{Error, ErrorKind, Write as _};
    use std::path::Path;

    use serde::{Deserialize, Serialize};

    #[cfg_attr(
        test,
        allow(dead_code, reason = "tests build this module beside the fcntl lock")
    )]
    pub(super) const CLOSE_BEFORE_REMOVE: bool = true;

    #[derive(Serialize, Deserialize)]
    struct PidLockMeta {
        #[serde(rename = "OwnerPID", default)]
        owner_pid: i64,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Status {
        Invalid,
        Locked,
        Unlocked,
        Stale,
    }

    /// `portableLockStatus`.
    pub(super) fn status(path: &Path, alive: impl Fn(i64) -> bool) -> Status {
        let Ok(data) = fs::read(path) else {
            return Status::Unlocked;
        };
        match serde_json::Deserializer::from_slice(&data)
            .into_iter::<PidLockMeta>()
            .next()
        {
            Some(Ok(meta)) if meta.owner_pid != 0 => {
                if alive(meta.owner_pid) {
                    Status::Locked
                } else {
                    Status::Stale
                }
            }
            _ => Status::Invalid,
        }
    }

    /// A file holding the owner's PID as JSON; a lock whose owner is gone is stale.
    pub(super) fn try_lock(path: &Path) -> std::io::Result<fs::File> {
        if fs::metadata(path).is_ok_and(|meta| meta.len() > 0) {
            match status(path, crate::sys::process_alive) {
                Status::Locked => {
                    return Err(Error::new(
                        ErrorKind::WouldBlock,
                        format!("file {:?} already locked", path.display().to_string()),
                    ));
                }
                Status::Stale => {
                    let _ignored = fs::remove_file(path);
                }
                Status::Invalid => {
                    return Err(Error::other(format!(
                        "can't Lock file {:?}: has invalid contents",
                        path.display().to_string()
                    )));
                }
                Status::Unlocked => {}
            }
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|err| {
                Error::new(
                    err.kind(),
                    format!("failed to create lock file {} {err}", path.display()),
                )
            })?;
        let mut owner = serde_json::to_vec(&PidLockMeta {
            owner_pid: i64::from(std::process::id()),
        })?;
        owner.push(b'\n');
        file.write_all(&owner)
            .map_err(|err| Error::new(err.kind(), format!("cannot write owner pid: {err}")))?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn matches_go_rules() {
        let dir = tempfile::tempdir().unwrap();
        let jar = CookieJar::open(dir.path().join("cookies")).unwrap();
        let now = OffsetDateTime::now_utc();
        jar.store(
            &url("https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate"),
            &[
                "a=1; Domain=.apple.com; Path=/; Max-Age=3600",
                "b=2",
                "c=3; Domain=com",
                "d=4; Domain=evil.com",
                "e=5; Secure; Path=/WebObjects; Max-Age=60",
            ],
            now,
        );
        assert_eq!(
            jar.header_for(
                &url("https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/x"),
                now
            )
            .as_deref(),
            Some("b=2; e=5; a=1")
        );
        assert_eq!(
            jar.header_for(&url("http://buy.itunes.apple.com/WebObjects/x"), now)
                .as_deref(),
            Some("a=1")
        );
        assert_eq!(
            jar.header_for(&url("https://apps.apple.com/"), now)
                .as_deref(),
            Some("a=1")
        );
        assert_eq!(
            default_path("/WebObjects/MZFinance.woa/wa/authenticate"),
            "/WebObjects/MZFinance.woa/wa"
        );
    }

    /// Reads a copy of a real Go-written jar: `IPATOOL_GO_COOKIES=<path>`.
    #[test]
    fn reads_go_jar() {
        let Ok(source) = std::env::var("IPATOOL_GO_COOKIES") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cookies");
        fs::copy(source, &path).unwrap();
        let before: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let jar = CookieJar::open(&path).unwrap();
        jar.save().unwrap();
        let after: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let ids = |value: &serde_json::Value| -> Vec<String> {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| format!("{}|{}|{}", entry["Domain"], entry["Path"], entry["Name"]))
                .collect()
        };
        // Go's unstable sort leaves full ties in any order, so compare as sets.
        let (mut before, mut after) = (ids(&before), ids(&after));
        before.sort();
        after.sort();
        assert_eq!(after, before);
    }

    #[test]
    fn saves_only_persistent_cookies_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cookies");
        let jar = CookieJar::open(&path).unwrap();
        let now = OffsetDateTime::now_utc();
        jar.store(
            &url("https://apple.com/"),
            &["p=1; Max-Age=600", "s=2"],
            now,
        );
        jar.save().unwrap();
        assert!(!lock_path(&path).exists());

        let other = CookieJar::open(&path).unwrap();
        other.store(&url("https://apple.com/"), &["q=3; Max-Age=600"], now);
        other.save().unwrap();

        let reloaded = CookieJar::open(&path).unwrap();
        assert_eq!(
            reloaded
                .header_for(&url("https://apple.com/"), now)
                .as_deref(),
            Some("p=1; q=3")
        );
    }

    /// `go4/lock`'s `TestLockPortable` cases, plus the stale and invalid owner files.
    #[test]
    fn portable_lock_like_go() {
        use portable_lock::{Status, status, try_lock};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cookies.lock");
        let file = try_lock(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{{\"OwnerPID\":{}}}\n", std::process::id())
        );
        let held = try_lock(&path).unwrap_err();
        assert_eq!(held.kind(), ErrorKind::WouldBlock);
        assert_eq!(
            held.to_string(),
            format!("file {:?} already locked", path.display().to_string())
        );
        drop(file);
        fs::remove_file(&path).unwrap();
        drop(try_lock(&path).unwrap());

        assert_eq!(
            status(&dir.path().join("missing"), |_| true),
            Status::Unlocked
        );
        for contents in ["", "garbage", "{\"OwnerPID\":0}", "{}"] {
            fs::write(&path, contents).unwrap();
            assert_eq!(status(&path, |_| true), Status::Invalid, "{contents}");
        }
        fs::write(&path, "{\"OwnerPID\":42}\n{").unwrap();
        assert_eq!(status(&path, |pid| pid == 42), Status::Locked);
        assert_eq!(status(&path, |_| false), Status::Stale);

        fs::write(&path, "{\"OwnerPID\":2147483647}").unwrap();
        drop(try_lock(&path).unwrap());
        fs::write(&path, "x").unwrap();
        assert_eq!(
            try_lock(&path).unwrap_err().to_string(),
            format!(
                "can't Lock file {:?}: has invalid contents",
                path.display().to_string()
            )
        );
    }

    #[test]
    fn process_liveness() {
        assert!(crate::sys::process_alive(i64::from(std::process::id())));
        assert!(!crate::sys::process_alive(i64::from(i32::MAX)));
    }
}
