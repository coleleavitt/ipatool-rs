//! The fork's `cmd/progress_test.go`.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ipatool_appstore::{Phase, ProgressHandle};
use serde_json::{Value, json};

use super::*;

/// A writer the test reads back.
#[derive(Clone, Default)]
pub(crate) struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for Buffer {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct FakeClock(Arc<Mutex<Instant>>);

impl FakeClock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }

    fn clock(&self) -> Clock {
        let now = Arc::clone(&self.0);
        Box::new(move || *now.lock().unwrap())
    }
}

fn reporter(mode: ProgressMode) -> (ProgressHandle, Buffer, FakeClock) {
    let (out, clock) = (Buffer::default(), FakeClock::new());
    let progress = new_progress(mode, Box::new(out.clone()), clock.clock()).unwrap();
    (progress, out, clock)
}

/// Go's `Bytes(current, total)`.
fn bytes(progress: &ProgressHandle, current: i64, total: i64) {
    progress.set_total(total);
    progress.set(current);
}

fn session(json: bool, interactive: bool, stdout_is_terminal: bool) -> ProgressEnvironment {
    ProgressEnvironment {
        json,
        interactive,
        stdout_is_terminal,
    }
}

#[test]
fn resolves_the_progress_mode() {
    for (flag, env, session, expected) in [
        (None, None, session(true, true, true), ProgressMode::None),
        (None, None, session(false, true, true), ProgressMode::Bar),
        (None, None, session(false, true, false), ProgressMode::Plain),
        (None, None, session(false, false, true), ProgressMode::Plain),
        (
            None,
            Some("none"),
            session(false, false, false),
            ProgressMode::None,
        ),
        (
            Some("json"),
            Some("plain"),
            session(false, false, false),
            ProgressMode::Json,
        ),
        (
            None,
            Some("auto"),
            session(false, true, true),
            ProgressMode::Bar,
        ),
        (
            None,
            Some("json"),
            session(true, false, false),
            ProgressMode::Json,
        ),
        (
            None,
            Some(""),
            session(false, true, true),
            ProgressMode::Bar,
        ),
    ] {
        assert_eq!(
            resolve_progress_mode(flag, env, &session),
            Ok(expected),
            "{flag:?} {env:?}"
        );
    }
}

#[test]
fn rejects_unknown_progress_modes() {
    assert_eq!(
        resolve_progress_mode(Some("fancy"), None, &session(false, false, false)),
        Err("invalid progress mode 'fancy'; use auto, bar, json, plain, or none".to_owned())
    );
}

#[test]
fn disables_progress_for_none() {
    assert!(
        new_progress(
            ProgressMode::None,
            Box::new(io::sink()),
            Box::new(Instant::now)
        )
        .is_none()
    );
}

#[test]
fn plain_prints_phases_once_and_byte_counts_per_percent_or_heartbeat() {
    let (progress, out, clock) = reporter(ProgressMode::Plain);
    progress.phase(Phase::Resolving);
    progress.phase(Phase::Resolving);
    progress.phase(Phase::Downloading);
    bytes(&progress, 0, 300_000_000);

    // Advanced, but within the minimum interval.
    clock.advance(Duration::from_millis(10));
    bytes(&progress, 15_000_000, 300_000_000);

    clock.advance(Duration::from_millis(20));
    bytes(&progress, 15_000_000, 300_000_000);

    // Less than a percent since the last line.
    clock.advance(Duration::from_secs(1));
    bytes(&progress, 16_500_000, 300_000_000);

    // Heartbeat while stalled at the same percent.
    clock.advance(Duration::from_secs(14));
    bytes(&progress, 16_500_000, 300_000_000);

    clock.advance(Duration::from_secs(1));
    bytes(&progress, 18_000_000, 300_000_000);

    progress.phase(Phase::Patching);

    assert_eq!(
        out.text(),
        [
            "resolving",
            "downloading",
            "downloading 0% (0 B / 300.0 MB)",
            "downloading 5% (15.0 MB / 300.0 MB)",
            "downloading 5% (16.5 MB / 300.0 MB)",
            "downloading 6% (18.0 MB / 300.0 MB)",
            "patching",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn plain_prints_byte_counts_on_an_interval_when_the_total_is_unknown() {
    let (progress, out, clock) = reporter(ProgressMode::Plain);
    progress.phase(Phase::Downloading);
    bytes(&progress, 0, -1);
    clock.advance(Duration::from_secs(15));
    bytes(&progress, 1_500, -1);
    assert_eq!(
        out.text(),
        "downloading\ndownloading 0 B\ndownloading 1.5 kB\n"
    );
}

#[test]
fn plain_counts_added_bytes_from_the_last_position() {
    let (progress, out, clock) = reporter(ProgressMode::Plain);
    progress.phase(Phase::Downloading);
    bytes(&progress, 100, 1000);
    clock.advance(Duration::from_millis(30));
    progress.add(400);
    assert_eq!(
        out.text(),
        "downloading\ndownloading 10% (100 B / 1.0 kB)\ndownloading 50% (500 B / 1.0 kB)\n"
    );
}

fn events(out: &Buffer) -> Vec<Value> {
    out.text()
        .lines()
        .map(|line| {
            let mut event: Value = serde_json::from_str(line).unwrap();
            let time = event.as_object_mut().unwrap().remove("time").unwrap();
            assert!(time.as_str().is_some_and(|time| time.len() >= 20), "{time}");
            event
        })
        .collect()
}

#[test]
fn json_emits_phases_and_throttled_byte_counts() {
    let (progress, out, clock) = reporter(ProgressMode::Json);
    progress.phase(Phase::Downloading);
    bytes(&progress, 0, 10);

    clock.advance(Duration::from_millis(100));
    bytes(&progress, 5, 10);

    clock.advance(Duration::from_millis(200));
    bytes(&progress, 6, 10);

    bytes(&progress, 10, 10);
    progress.phase(Phase::Patching);

    assert_eq!(
        events(&out),
        [
            json!({"level": "info", "event": "progress", "phase": "downloading"}),
            json!({"level": "info", "event": "progress", "phase": "downloading", "bytes": 0, "total": 10}),
            json!({"level": "info", "event": "progress", "phase": "downloading", "bytes": 6, "total": 10}),
            json!({"level": "info", "event": "progress", "phase": "downloading", "bytes": 10, "total": 10}),
            json!({"level": "info", "event": "progress", "phase": "patching"}),
        ]
    );
    // zerolog's field order: level first, time last.
    let first = out.text().lines().nth(1).unwrap().to_owned();
    assert!(
        first.starts_with(r#"{"level":"info","event":"progress","phase":"downloading","bytes":0,"total":10,"time":""#),
        "{first}"
    );
}

#[test]
fn json_omits_unknown_totals() {
    let (progress, out, _) = reporter(ProgressMode::Json);
    progress.phase(Phase::Downloading);
    bytes(&progress, 3, -1);
    assert_eq!(
        events(&out)[1],
        json!({"level": "info", "event": "progress", "phase": "downloading", "bytes": 3})
    );
}

#[test]
fn formats_byte_counts() {
    for (value, expected) in [
        (999, "999 B"),
        (1_500, "1.5 kB"),
        (120_300_000, "120.3 MB"),
        (2_000_000_000, "2.0 GB"),
    ] {
        assert_eq!(format_bytes(value), expected);
    }
}
