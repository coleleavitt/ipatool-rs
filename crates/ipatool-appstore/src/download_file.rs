//! Go's `downloadFile` (`appstore_download.go`), `appstore_download_parallel.go` and
//! `appstore_download_range.go`: a resumable package download through the `transfer`
//! executor, split into four ranged requests when at least 128 MiB remain.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ipatool_store::http::{Header, HttpRequest, HttpResponse};

use crate::download_types::ProgressHandle;
use crate::{Client, Error, Result};

/// `parallelDownloadThreshold` and `parallelDownloadCount`.
pub(crate) const PARALLEL_THRESHOLD: i64 = 128 << 20;
const PARALLEL_COUNT: i64 = 4;
const TRANSFER: &str = "transfer";

/// Go's `downloadPartsPath`.
pub(crate) fn parts_path(destination: &Path) -> PathBuf {
    let mut path = destination.as_os_str().to_owned();
    path.push(".ipatool-parts");
    PathBuf::from(path)
}

/// Go's `downloadValidator`: only a strong, quoted `ETag`.
fn validator(headers: &Header) -> String {
    let etag = headers.get("ETag");
    if etag.starts_with('"') && etag.ends_with('"') {
        etag.to_owned()
    } else {
        String::new()
    }
}

/// Go's `downloadResponseRange` result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResponseRange {
    pub offset: i64,
    pub remaining: i64,
    pub total: i64,
    pub complete: bool,
}

/// Go's `downloadResponseRange`: validates the response before any partial bytes change.
pub(crate) fn response_range(
    status: u16,
    headers: &Header,
    content_length: i64,
    local_size: i64,
) -> Result<ResponseRange> {
    let content_range = headers.get("Content-Range");
    match status {
        200 => Ok(ResponseRange {
            offset: 0,
            remaining: content_length,
            total: content_length,
            complete: false,
        }),
        416 => {
            let size = content_range
                .strip_prefix("bytes */")
                .and_then(|value| value.parse::<i64>().ok());
            match size {
                Some(size) if size >= 0 && size == local_size => Ok(ResponseRange {
                    offset: local_size,
                    remaining: 0,
                    total: size,
                    complete: true,
                }),
                _ => Err(Error::Invalid(format!(
                    "download range rejected: local size {local_size} does not match server range {}",
                    go_quote(content_range)
                ))),
            }
        }
        206 => {
            let parsed = content_range.strip_prefix("bytes ").and_then(|value| {
                let (bounds, total) = value.split_once('/')?;
                let (start, end) = bounds.split_once('-')?;
                Some((
                    start.parse::<i64>().ok()?,
                    end.parse::<i64>().ok()?,
                    total.parse::<i64>().ok()?,
                ))
            });
            let Some((start, end, size)) = parsed.filter(|&(start, end, size)| {
                start == local_size && start >= 0 && end >= start && size > end
            }) else {
                return Err(Error::Invalid(format!(
                    "invalid download content range {} for local size {local_size}",
                    go_quote(content_range)
                )));
            };
            let length = end - start + 1;
            if content_length >= 0 && content_length != length {
                return Err(Error::Invalid(format!(
                    "download content length {content_length} does not match range length {length}"
                )));
            }
            Ok(ResponseRange {
                offset: start,
                remaining: length,
                total: size,
                complete: false,
            })
        }
        status => Err(Error::Invalid(format!(
            "unexpected download response status: {status}"
        ))),
    }
}

/// Go's `%q` of a header value.
fn go_quote(text: &str) -> String {
    ipatool_store::gourl::go_quote(text.as_bytes())
}

/// A file write that also counts progress, as Go's `io.MultiWriter(file, progress)`, and
/// stops when another range failed (Go cancels the shared context).
struct Sink<'a, W> {
    inner: W,
    progress: Option<&'a ProgressHandle>,
    cancel: &'a AtomicBool,
}

impl<W: Write> Write for Sink<'_, W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("context canceled"));
        }
        let written = self.inner.write(data)?;
        if let Some(progress) = self.progress {
            progress.add(written as u64);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Go's `io.Copy` of at most `limit` bytes (`None`: until EOF); returns the bytes written.
fn copy_limited(reader: impl Read, writer: &mut impl Write, limit: Option<u64>) -> io::Result<u64> {
    if let Some(limit) = limit {
        io::copy(&mut reader.take(limit), writer)
    } else {
        let mut reader = reader;
        io::copy(&mut reader, writer)
    }
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF")
}

/// A parallel attempt's failure, with the validator the fallback resumes under.
struct ParallelFailure {
    error: Error,
    validator: String,
}

enum AttemptError {
    Parallel(ParallelFailure),
    Other(Error),
}

impl From<Error> for AttemptError {
    fn from(error: Error) -> Self {
        Self::Other(error)
    }
}

fn io_error(context: &str) -> impl FnOnce(io::Error) -> Error + '_ {
    move |error| Error::io(context, error)
}

impl Client {
    /// Go's `downloadFile`: resumes `dst` from its length; a failed parallel attempt is
    /// retried once over a single connection with the first attempt's validator.
    ///
    /// # Errors
    /// Go's request, range and file errors; both attempts' errors when the fallback fails.
    pub async fn download_file(
        &self,
        src: &str,
        dst: &Path,
        progress: Option<ProgressHandle>,
    ) -> Result<()> {
        let client = self.clone();
        let (src, dst) = (src.to_owned(), dst.to_owned());
        tokio::task::spawn_blocking(move || {
            client.download_file_blocking(&src, &dst, progress.as_ref())
        })
        .await
        .map_err(|error| Error::Invalid(format!("download task failed: {error}")))?
    }

    /// [`Client::download_file`] on the calling thread.
    ///
    /// # Errors
    /// See [`Client::download_file`].
    pub fn download_file_blocking(
        &self,
        src: &str,
        dst: &Path,
        progress: Option<&ProgressHandle>,
    ) -> Result<()> {
        match self.download_attempt(src, dst, progress, true, "") {
            Ok(()) => Ok(()),
            Err(AttemptError::Other(error)) => Err(error),
            Err(AttemptError::Parallel(failure)) => {
                let first = Error::Invalid(format!("parallel download failed: {}", failure.error));
                match self.download_attempt(src, dst, progress, false, &failure.validator) {
                    Ok(()) => Ok(()),
                    Err(
                        AttemptError::Other(error)
                        | AttemptError::Parallel(ParallelFailure { error, .. }),
                    ) => Err(Error::Joined(vec![
                        first,
                        error.context("single-connection fallback failed"),
                    ])),
                }
            }
        }
    }

    /// Go's `downloadFileAttempt`.
    fn download_attempt(
        &self,
        src: &str,
        dst: &Path,
        progress: Option<&ProgressHandle>,
        parallel: bool,
        if_range: &str,
    ) -> std::result::Result<(), AttemptError> {
        // Staged ranges are expendable; only the contiguous destination is resumed.
        remove_all(&parts_path(dst)).map_err(io_error("failed to remove stale download parts"))?;
        let mut headers = Header::new();
        headers.set("Accept-Encoding", "identity");
        if !if_range.is_empty() {
            headers.set("If-Range", if_range);
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dst)
            .map_err(io_error("failed to open file"))?;
        let local_size = fs::metadata(dst)
            .map_err(io_error("failed to get file info"))?
            .len();
        let local_size = i64::try_from(local_size).unwrap_or(i64::MAX);
        headers.add("Range", format!("bytes={local_size}-"));
        let response = self
            .store_http(
                TRANSFER,
                HttpRequest {
                    method: "GET".to_owned(),
                    url: src.to_owned(),
                    headers,
                    body: Vec::new(),
                },
            )
            .map_err(|error| error.context("request failed"))?;
        let range = response_range(
            response.status,
            &response.headers,
            response.content_length,
            local_size,
        )?;
        if range.complete {
            return Ok(());
        }
        if response.status == 200 {
            file.set_len(0)
                .map_err(io_error("failed to restart download"))?;
        }
        file.seek(SeekFrom::Start(
            u64::try_from(range.offset).unwrap_or_default(),
        ))
        .map_err(io_error("can not seek file"))?;
        if let Some(progress) = progress {
            progress.set_total(range.total);
            progress.set(range.offset);
        }
        let response_validator = validator(&response.headers);
        // Combining responses requires a strong validator to keep all ranges on the same
        // representation. Servers without one keep the single stream.
        if parallel
            && response.status == 206
            && range.remaining == range.total - range.offset
            && range.remaining >= PARALLEL_THRESHOLD
            && !response_validator.is_empty()
        {
            return self
                .download_parallel(
                    src,
                    dst,
                    &mut file,
                    response,
                    range,
                    progress,
                    &response_validator,
                )
                .map_err(|error| {
                    AttemptError::Parallel(ParallelFailure {
                        error,
                        validator: response_validator.clone(),
                    })
                });
        }
        let cancel = AtomicBool::new(false);
        let mut sink = Sink {
            inner: &mut file,
            progress,
            cancel: &cancel,
        };
        let limit = u64::try_from(range.remaining).ok();
        let written = copy_limited(response.body, &mut sink, limit)
            .map_err(io_error("failed to write file"))?;
        let written = i64::try_from(written).unwrap_or(i64::MAX);
        if (range.remaining >= 0 && written != range.remaining)
            || (range.total >= 0 && range.offset + written != range.total)
        {
            return Err(Error::io("download is incomplete", eof()).into());
        }
        Ok(())
    }

    /// Go's `downloadFileParallel`: the first quarter comes from the open response; three
    /// ranged requests stage the rest beside the destination, which is appended to only in
    /// order, so its length always describes a valid prefix for the next attempt.
    #[allow(clippy::too_many_arguments, reason = "Go's parameter list")]
    fn download_parallel(
        &self,
        src: &str,
        dst: &Path,
        file: &mut File,
        response: HttpResponse,
        range: ResponseRange,
        progress: Option<&ProgressHandle>,
        validator: &str,
    ) -> Result<()> {
        let directory = parts_path(dst);
        create_private_dir(&directory)
            .map_err(io_error("failed to create download staging directory"))?;
        let result =
            self.download_parts(src, &directory, file, response, range, progress, validator);
        match (result, remove_all(&directory)) {
            (result, Ok(())) => result,
            (Ok(()), Err(cleanup)) => Err(Error::io(
                "failed to remove download staging directory",
                cleanup,
            )),
            (Err(error), Err(cleanup)) => Err(Error::Joined(vec![
                error,
                Error::io("failed to remove download staging directory", cleanup),
            ])),
        }
    }

    #[allow(clippy::too_many_arguments, reason = "Go's parameter list")]
    fn download_parts(
        &self,
        src: &str,
        directory: &Path,
        file: &mut File,
        response: HttpResponse,
        range: ResponseRange,
        progress: Option<&ProgressHandle>,
        validator: &str,
    ) -> Result<()> {
        let (offset, total) = (range.offset, range.total);
        let chunk = (total - offset) / PARALLEL_COUNT;
        let cancel = AtomicBool::new(false);
        let (failures, copy_error) = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..PARALLEL_COUNT - 1)
                .map(|index| {
                    let cancel = &cancel;
                    scope.spawn(move || {
                        let start = offset + chunk * (index + 1);
                        let end = if index == PARALLEL_COUNT - 2 {
                            total - 1
                        } else {
                            start + chunk - 1
                        };
                        let path = directory.join(index.to_string());
                        let result = self.download_range(
                            src, &path, start, end, total, validator, progress, cancel,
                        );
                        if result.is_err() {
                            cancel.store(true, Ordering::Relaxed);
                        }
                        result
                    })
                })
                .collect();
            let mut sink = Sink {
                inner: &mut *file,
                progress,
                cancel: &cancel,
            };
            // The first request covers the whole remainder; stop reading it after its
            // first quarter.
            let chunk_bytes = u64::try_from(chunk).unwrap_or_default();
            let copy_error = match copy_limited(response.body, &mut sink, Some(chunk_bytes)) {
                Ok(written) if written == chunk_bytes => None,
                Ok(_) => Some(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF")),
                Err(error) => Some(error),
            };
            if copy_error.is_some() {
                cancel.store(true, Ordering::Relaxed);
            }
            let failures: Vec<Result<()>> = workers
                .into_iter()
                .map(|worker| {
                    worker.join().unwrap_or_else(|_| {
                        Err(Error::Invalid("download range panicked".to_owned()))
                    })
                })
                .collect();
            (failures, copy_error)
        });
        for failure in failures {
            failure?;
        }
        if let Some(error) = copy_error {
            return Err(Error::io("failed to download first range", error));
        }
        for index in 0..PARALLEL_COUNT - 1 {
            append_part(file, &directory.join(index.to_string()))?;
        }
        Ok(())
    }

    /// Go's `downloadFileRange`.
    #[allow(clippy::too_many_arguments, reason = "Go's parameter list")]
    fn download_range(
        &self,
        src: &str,
        dst: &Path,
        start: i64,
        end: i64,
        total: i64,
        validator: &str,
        progress: Option<&ProgressHandle>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut headers = Header::new();
        headers.set("Accept-Encoding", "identity");
        headers.set("Range", format!("bytes={start}-{end}"));
        if !validator.is_empty() {
            headers.set("If-Range", validator);
        }
        let response = self
            .store_http(
                TRANSFER,
                HttpRequest {
                    method: "GET".to_owned(),
                    url: src.to_owned(),
                    headers,
                    body: Vec::new(),
                },
            )
            .map_err(|error| error.context("range request failed"))?;
        let range = response_range(
            response.status,
            &response.headers,
            response.content_length,
            start,
        )?;
        if response.status != 206 || range.remaining != end - start + 1 || range.total != total {
            return Err(Error::Invalid(format!(
                "unexpected download range {}",
                go_quote(response.headers.get("Content-Range"))
            )));
        }
        let encoding = response.headers.get("Content-Encoding");
        if !encoding.is_empty() && !encoding.eq_ignore_ascii_case("identity") {
            return Err(Error::Invalid(format!(
                "unexpected download content encoding {}",
                go_quote(encoding)
            )));
        }
        let etag = response.headers.get("ETag");
        if !validator.is_empty() && !etag.is_empty() && etag != validator {
            return Err(Error::Invalid(format!(
                "download validator changed from {} to {}",
                go_quote(validator),
                go_quote(etag)
            )));
        }
        let mut file = create_part(dst).map_err(io_error("failed to create download part"))?;
        let mut sink = Sink {
            inner: &mut file,
            progress,
            cancel,
        };
        let length = range.remaining;
        let written = copy_limited(
            response.body,
            &mut sink,
            Some(u64::try_from(length + 1).unwrap_or_default()),
        )
        .map_err(io_error("failed to write download part"))?;
        if i64::try_from(written).unwrap_or(i64::MAX) != length {
            return Err(Error::io("download part is incomplete", eof()));
        }
        Ok(())
    }
}

/// Go's `appendDownloadPart`.
fn append_part(destination: &mut File, path: &Path) -> Result<()> {
    let mut part = File::open(path).map_err(io_error("failed to open download part"))?;
    io::copy(&mut part, destination).map_err(io_error("failed to append download part"))?;
    drop(part);
    fs::remove_file(path).map_err(io_error("failed to remove download part"))
}

/// Go's `os.RemoveAll`: a missing path is not an error.
fn remove_all(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}

/// `os.Mkdir(path, 0700)`.
fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// `os.OpenFile(path, O_CREATE|O_EXCL|O_WRONLY, 0600)`.
fn create_part(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}
