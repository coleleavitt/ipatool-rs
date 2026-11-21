//! Unicorn arm64 engine setup and bounded execution. Port of Go `internal/sap/unicorn`.
//!
//! Go loads a downloaded Unicorn library at run time; here Unicorn is built from source and
//! linked, so only the configuration, the shared-clock guard and the error text remain.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use unicorn_engine::{Arch, ControlType, Mode, Query, Unicorn, uc_error};

use crate::error::{Error, Result};

const CPU_MODEL_MAX: i32 = 3;
const TCG_BUFFER_SIZE: u32 = 4 << 20;
const CTL_TCG_BUFFER_SIZE_WRITE: u32 = 0x0d | 1 << 26 | 1 << 30;

/// Go's `Engine.err`: `unicorn error <code>: <description>`.
#[must_use]
pub fn unicorn_error(error: uc_error) -> Error {
    Error::msg(format!("unicorn error {}: {error}", error as i32))
}

pub trait UcResult<T> {
    fn uc(self) -> Result<T>;
}

impl<T> UcResult<T> for std::result::Result<T, uc_error> {
    fn uc(self) -> Result<T> {
        self.map_err(unicorn_error)
    }
}

/// Unicorn's `machine_initialize` briefly switches the process-wide clock to wall time, which
/// can expire another engine's timeout. Engines initialize only while none is executing.
struct ClockState {
    active: Mutex<usize>,
    idle: Condvar,
}

static CLOCK: ClockState = ClockState {
    active: Mutex::new(0),
    idle: Condvar::new(),
};

struct Emulating;

impl Emulating {
    fn begin() -> Self {
        *CLOCK.active.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        Self
    }
}

impl Drop for Emulating {
    fn drop(&mut self) {
        let mut active = CLOCK.active.lock().unwrap_or_else(PoisonError::into_inner);
        *active -= 1;
        if *active == 0 {
            CLOCK.idle.notify_all();
        }
    }
}

/// Creates an arm64 engine with Unicorn's most capable CPU model and a bounded JIT buffer,
/// forcing Unicorn's lazy initialization before it is returned.
pub fn new<'a, D: 'a>(data: D) -> Result<Unicorn<'a, D>> {
    let mut engine = Unicorn::new_with_data(Arch::ARM64, Mode::ARM, data)
        .map_err(|error| prefixed("create arm64 emulator", error))?;
    engine
        .ctl_set_cpu_model(CPU_MODEL_MAX)
        .map_err(|error| prefixed("configure arm64 CPU model", error))?;
    set_tcg_buffer_size(&engine)
        .map_err(|error| prefixed("configure Unicorn translation buffer", error))?;
    let active = CLOCK.active.lock().unwrap_or_else(PoisonError::into_inner);
    let _idle = CLOCK
        .idle
        .wait_while(active, |active| *active != 0)
        .unwrap_or_else(PoisonError::into_inner);
    engine
        .query(Query::PAGE_SIZE)
        .map_err(|error| prefixed("initialize arm64 emulator", error))?;
    Ok(engine)
}

fn prefixed(context: &str, error: uc_error) -> Error {
    Error::msg(format!("{context}: {}", unicorn_error(error)))
}

#[allow(unsafe_code)]
fn set_tcg_buffer_size<D>(engine: &Unicorn<'_, D>) -> std::result::Result<(), uc_error> {
    // SAFETY: `get_handle` is the engine's open `uc_engine`, valid while `engine` is borrowed.
    // In write mode `uc_ctl` reads exactly one `uint32_t` variadic argument for
    // UC_CTL_TCG_BUFFER_SIZE (unicorn-engine-sys 2.1.5 `uc.c`), and `TCG_BUFFER_SIZE` is a `u32`.
    unsafe {
        unicorn_engine::uc_ctl(
            engine.get_handle(),
            ControlType(CTL_TCG_BUFFER_SIZE_WRITE),
            TCG_BUFFER_SIZE,
        )
    }
    .into()
}

/// Runs from `begin` until `end`, failing when `timeout` passes. `instruction_limit` 0 is no limit.
pub fn start_bounded<D>(
    engine: &mut Unicorn<'_, D>,
    begin: u64,
    end: u64,
    timeout: Duration,
    instruction_limit: usize,
) -> Result<()> {
    if timeout.is_zero() {
        return Err(Error::msg("unicorn timeout must be positive"));
    }
    let microseconds = u64::try_from(timeout.as_micros())
        .unwrap_or(u64::MAX)
        .max(1);
    let emulating = Emulating::begin();
    let result = engine.emu_start(begin, end, microseconds, instruction_limit);
    drop(emulating);
    result.uc()?;
    let timed_out = engine
        .query(Query::TIMEOUT)
        .map_err(|error| Error::msg(format!("query Unicorn timeout: {}", unicorn_error(error))))?;
    if timed_out != 0 {
        return Err(Error::msg(format!(
            "unicorn emulation timed out after {}",
            go_duration(timeout)
        )));
    }
    Ok(())
}

/// Go's `time.Duration.String` for whole-second and sub-second values.
fn go_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let nanos = duration.subsec_nanos();
    if total == 0 {
        return match nanos {
            0 => "0s".to_owned(),
            n if n < 1_000 => format!("{n}ns"),
            n if n < 1_000_000 => format!("{}µs", trim(f64::from(n) / 1e3)),
            n => format!("{}ms", trim(f64::from(n) / 1e6)),
        };
    }
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    let seconds = if nanos == 0 {
        seconds.to_string()
    } else {
        trim(f64::from(u32::try_from(seconds).unwrap_or_default()) + f64::from(nanos) / 1e9)
    };
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, minutes) => format!("{minutes}m{seconds}s"),
        (hours, minutes) => format!("{hours}h{minutes}m{seconds}s"),
    }
}

fn trim(value: f64) -> String {
    let text = format!("{value:.9}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use unicorn_engine::{Prot, RegisterARM64};

    use super::*;

    #[test]
    fn runs_arm64_with_go_configuration() {
        let mut engine = new(()).unwrap();
        assert_eq!(engine.ctl_get_cpu_model().unwrap(), CPU_MODEL_MAX);
        engine.mem_map(0x1000, 0x1000, Prot::ALL).unwrap();
        engine.mem_write(0x1000, &[0x40, 0x05, 0x80, 0xd2]).unwrap();
        start_bounded(&mut engine, 0x1000, 0x1004, Duration::from_secs(1), 0).unwrap();
        assert_eq!(engine.reg_read(RegisterARM64::X0).unwrap(), 42);
    }

    #[test]
    fn reports_timeout_like_go() {
        let mut engine = new(()).unwrap();
        engine.mem_map(0x1000, 0x1000, Prot::ALL).unwrap();
        // b . (branch to self)
        engine.mem_write(0x1000, &[0x00, 0x00, 0x00, 0x14]).unwrap();
        let error =
            start_bounded(&mut engine, 0x1000, 0x2000, Duration::from_millis(20), 0).unwrap_err();
        assert_eq!(error.to_string(), "unicorn emulation timed out after 20ms");
        let error = engine
            .mem_read_as_vec(0x9000_0000, 4)
            .map_err(unicorn_error)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unicorn error 6: Invalid memory read (UC_ERR_READ_UNMAPPED)"
        );
    }

    #[test]
    fn formats_durations_like_go() {
        assert_eq!(go_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(go_duration(Duration::from_secs(120)), "2m0s");
        assert_eq!(go_duration(Duration::from_secs(30)), "30s");
        assert_eq!(go_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(go_duration(Duration::from_millis(20)), "20ms");
    }
}
