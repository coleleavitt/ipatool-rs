//! Download progress (the fork's `cmd/progress.go`, majd/ipatool#649): the terminal bar,
//! JSON lines and plain lines for scripts and agents, and the mode that picks one.

use std::io::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ipatool_appstore::{Phase, Progress, ProgressHandle};
use serde_json::{Map, Value};

pub(crate) const PROGRESS_ENVIRONMENT_VARIABLE: &str = "IPATOOL_PROGRESS";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProgressMode {
    Bar,
    Json,
    Plain,
    None,
}

const JSON_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const PLAIN_PROGRESS_HEARTBEAT: Duration = Duration::from_secs(15);
/// Downloads lasting at least 3 seconds report every percent.
const PLAIN_PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(30);
const PLAIN_PROGRESS_PERCENT_STEP: i64 = 1;

/// Go's `progressEnvironment`.
pub(crate) struct ProgressEnvironment {
    pub json: bool,
    pub interactive: bool,
    pub stdout_is_terminal: bool,
}

/// Go's `resolveProgressMode`: the flag, then the environment variable, then the session.
pub(crate) fn resolve_progress_mode(
    flag: Option<&str>,
    env: Option<&str>,
    session: &ProgressEnvironment,
) -> Result<ProgressMode, String> {
    let value = flag
        .or_else(|| env.filter(|value| !value.is_empty()))
        .unwrap_or("auto");
    match value {
        "bar" => Ok(ProgressMode::Bar),
        "json" => Ok(ProgressMode::Json),
        "plain" => Ok(ProgressMode::Plain),
        "none" => Ok(ProgressMode::None),
        // Existing JSON consumers may read stdout and stderr together, so JSON progress
        // is opt-in.
        "auto" => Ok(if session.json {
            ProgressMode::None
        } else if session.interactive && session.stdout_is_terminal {
            ProgressMode::Bar
        } else {
            ProgressMode::Plain
        }),
        _ => Err(format!(
            "invalid progress mode '{value}'; use auto, bar, json, plain, or none"
        )),
    }
}

pub(crate) type Clock = Box<dyn Fn() -> Instant + Send + Sync>;

/// Go's `newProgress`: the bar draws on stdout, JSON and plain lines go to `stderr`.
pub(crate) fn new_progress(
    mode: ProgressMode,
    stderr: Box<dyn Write + Send>,
    now: Clock,
) -> Option<ProgressHandle> {
    let lines = |format| -> ProgressHandle {
        std::sync::Arc::new(Lines {
            format,
            now,
            state: Mutex::new(LineState {
                out: stderr,
                phase: None,
                last_emit: None,
                last_percent: 0,
                current: 0,
                total: -1,
            }),
        })
    };
    match mode {
        ProgressMode::Bar => Some(std::sync::Arc::new(ProgressBar::new("downloading"))),
        ProgressMode::Json => Some(lines(LineFormat::Json)),
        ProgressMode::Plain => Some(lines(LineFormat::Plain)),
        ProgressMode::None => None,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Copy)]
enum LineFormat {
    /// Go's `jsonProgress`: zerolog lines, at most four byte counts per second.
    Json,
    /// Go's `plainProgress`: one line per phase, then byte counts per percent and as a
    /// heartbeat while stalled.
    Plain,
}

struct Lines {
    format: LineFormat,
    now: Clock,
    state: Mutex<LineState>,
}

struct LineState {
    out: Box<dyn Write + Send>,
    phase: Option<Phase>,
    last_emit: Option<Instant>,
    last_percent: i64,
    current: i64,
    /// Negative when unknown.
    total: i64,
}

impl LineState {
    fn phase_name(&self) -> &'static str {
        self.phase.map_or("", Phase::as_str)
    }

    fn json(&mut self, fields: Map<String, Value>) {
        let mut line = Map::new();
        line.insert("level".into(), "info".into());
        line.insert("event".into(), "progress".into());
        line.insert("phase".into(), self.phase_name().into());
        line.extend(fields);
        line.insert("time".into(), crate::output::zerolog_time().into());
        let _ = writeln!(self.out, "{}", Value::Object(line));
    }
}

impl Lines {
    fn bytes(&self, state: &mut LineState) {
        let now = (self.now)();
        match self.format {
            LineFormat::Json => {
                let finished = state.total >= 0 && state.current >= state.total;
                if !finished
                    && state
                        .last_emit
                        .is_some_and(|last| now - last < JSON_PROGRESS_INTERVAL)
                {
                    return;
                }
                state.last_emit = Some(now);
                let mut fields = Map::new();
                fields.insert("bytes".into(), state.current.into());
                if state.total >= 0 {
                    fields.insert("total".into(), state.total.into());
                }
                state.json(fields);
            }
            LineFormat::Plain => {
                let percent = if state.total > 0 {
                    state.current * 100 / state.total
                } else {
                    0
                };
                if let Some(last) = state.last_emit {
                    let elapsed = now - last;
                    let advanced = state.total > 0
                        && percent - state.last_percent >= PLAIN_PROGRESS_PERCENT_STEP;
                    if elapsed < PLAIN_PROGRESS_HEARTBEAT
                        && (!advanced || elapsed < PLAIN_PROGRESS_MIN_INTERVAL)
                    {
                        return;
                    }
                }
                state.last_emit = Some(now);
                state.last_percent = percent;
                let phase = state.phase_name();
                let line = if state.total >= 0 {
                    format!(
                        "{phase} {percent}% ({} / {})",
                        format_bytes(state.current),
                        format_bytes(state.total)
                    )
                } else {
                    format!("{phase} {}", format_bytes(state.current))
                };
                let _ = writeln!(state.out, "{line}");
            }
        }
    }
}

impl Progress for Lines {
    fn phase(&self, phase: Phase) {
        let mut state = lock(&self.state);
        if state.phase == Some(phase) {
            return;
        }
        state.phase = Some(phase);
        state.last_emit = None;
        match self.format {
            LineFormat::Json => state.json(Map::new()),
            LineFormat::Plain => {
                let _ = writeln!(state.out, "{}", phase.as_str());
            }
        }
    }

    /// Reported with the next position, as Go's `Bytes(current, total)`.
    fn set_total(&self, total: i64) {
        lock(&self.state).total = total;
    }

    fn set(&self, position: i64) {
        let mut state = lock(&self.state);
        state.current = position;
        self.bytes(&mut state);
    }

    fn add(&self, bytes: u64) {
        let mut state = lock(&self.state);
        state.current = state
            .current
            .saturating_add(i64::try_from(bytes).unwrap_or(i64::MAX));
        self.bytes(&mut state);
    }
}

/// Go's `formatBytes` (SI units, one decimal from 1 kB).
pub(crate) fn format_bytes(value: i64) -> String {
    #[allow(clippy::cast_precision_loss, reason = "display only")]
    format_bytes_f(value.max(0) as f64)
}

fn format_bytes_f(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = value;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A terminal progress bar in the spirit of Go's `progressbar` options: description,
/// percentage, bar, byte counts and rate, redrawn at most every 65 ms and cleared when
/// done. Like the fork's `barProgress`, it starts with the `downloading` phase.
pub(crate) struct ProgressBar {
    description: &'static str,
    state: Mutex<BarState>,
}

struct BarState {
    active: bool,
    total: i64,
    position: i64,
    started: Instant,
    drawn: Option<Instant>,
    visible: bool,
}

const THROTTLE: Duration = Duration::from_millis(65);
const BAR_WIDTH: usize = 20;

impl ProgressBar {
    fn new(description: &'static str) -> Self {
        Self {
            description,
            state: Mutex::new(BarState {
                active: false,
                total: 1,
                position: 0,
                started: Instant::now(),
                drawn: None,
                visible: false,
            }),
        }
    }

    fn draw(&self, state: &mut BarState, force: bool) {
        if !state.active {
            return;
        }
        let now = Instant::now();
        if !force && state.drawn.is_some_and(|drawn| now - drawn < THROTTLE) {
            return;
        }
        state.drawn = Some(now);
        let mut stdout = std::io::stdout().lock();
        if state.total > 0 && state.position >= state.total {
            // OptionClearOnFinish.
            if state.visible {
                let _ = write!(stdout, "\r\x1b[K");
                let _ = stdout.flush();
                state.visible = false;
            }
            return;
        }
        #[allow(clippy::cast_precision_loss, reason = "display only")]
        let fraction = if state.total > 0 {
            (state.position as f64 / state.total as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "0..=width"
        )]
        let filled = (fraction * BAR_WIDTH as f64) as usize;
        let elapsed = now.duration_since(state.started).as_secs_f64().max(0.001);
        #[allow(clippy::cast_precision_loss, reason = "display only")]
        let rate = state.position as f64 / elapsed;
        let _ = write!(
            stdout,
            "\r{} {:3.0}% |{}{}| ({}/{}, {}/s)\x1b[K",
            self.description,
            fraction * 100.0,
            "█".repeat(filled),
            " ".repeat(BAR_WIDTH - filled),
            format_bytes(state.position),
            format_bytes(state.total),
            format_bytes_f(rate),
        );
        let _ = stdout.flush();
        state.visible = true;
    }
}

impl Progress for ProgressBar {
    fn phase(&self, phase: Phase) {
        if phase == Phase::Downloading {
            let mut state = lock(&self.state);
            *state = BarState {
                active: true,
                total: 1,
                position: 0,
                started: Instant::now(),
                drawn: None,
                visible: false,
            };
        }
    }

    fn set_total(&self, total: i64) {
        let mut state = lock(&self.state);
        state.total = total;
        self.draw(&mut state, true);
    }

    fn set(&self, position: i64) {
        let mut state = lock(&self.state);
        state.position = position;
        state.started = Instant::now();
        self.draw(&mut state, true);
    }

    fn add(&self, bytes: u64) {
        let mut state = lock(&self.state);
        state.position += i64::try_from(bytes).unwrap_or(i64::MAX);
        let done = state.position >= state.total;
        self.draw(&mut state, done);
    }
}

#[cfg(test)]
pub(crate) mod tests;
