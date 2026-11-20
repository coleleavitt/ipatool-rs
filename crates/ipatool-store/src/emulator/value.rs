//! Go's `Object.Value any` (guest objects) and the host values `Export`/`importValue` exchange.

use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
thread_local! {
    /// The wall time the bridge reads on this thread, for the Go oracle comparison.
    pub(crate) static TEST_NOW: std::cell::Cell<Option<Date>> = const { std::cell::Cell::new(None) };
    /// A counter that replaces host randomness on this thread, for the Go oracle comparison.
    pub(crate) static TEST_RANDOM: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

/// Go's `time.Time` as the bridge uses it: an instant with nanosecond precision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    pub seconds: i64,
    pub nanos: u32,
}

impl Date {
    /// Go's `time.Unix(seconds, nanos)`, normalizing `nanos` into `0..1e9`.
    #[must_use]
    pub fn unix(seconds: i64, nanos: i64) -> Self {
        let seconds = seconds.wrapping_add(nanos.div_euclid(1_000_000_000));
        let nanos = u32::try_from(nanos.rem_euclid(1_000_000_000)).unwrap_or_default();
        Self { seconds, nanos }
    }

    #[must_use]
    pub fn now() -> Self {
        #[cfg(test)]
        if let Some(now) = TEST_NOW.get() {
            return now;
        }
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => Self {
                seconds: i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
                nanos: elapsed.subsec_nanos(),
            },
            Err(before) => {
                let elapsed = before.duration();
                Self::unix(
                    -i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
                    -i64::from(elapsed.subsec_nanos()),
                )
            }
        }
    }

    /// Go's `t.Add(d)` for whole seconds.
    #[must_use]
    pub fn add_seconds(self, seconds: i64) -> Self {
        Self {
            seconds: self.seconds.wrapping_add(seconds),
            ..self
        }
    }

    /// Go's `t.Sub(u).Seconds()`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "Go's Duration.Seconds is a float64"
    )]
    pub fn seconds_since(self, other: Self) -> f64 {
        let nanos = (i128::from(self.seconds) - i128::from(other.seconds)) * 1_000_000_000
            + i128::from(self.nanos)
            - i128::from(other.nanos);
        // Go's Duration saturates at the int64 range.
        let nanos = nanos.clamp(i128::from(i64::MIN), i128::from(i64::MAX));
        let nanos = i64::try_from(nanos).unwrap_or_default();
        let whole = nanos / 1_000_000_000;
        let fraction = nanos % 1_000_000_000;
        whole as f64 + fraction as f64 / 1e9
    }

    /// `float64(t.Unix()) + float64(t.Nanosecond())/1e9`.
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "Go converts to float64")]
    pub fn interval(self) -> f64 {
        self.seconds as f64 + f64::from(self.nanos) / 1e9
    }

    /// Go's `time.Unix(int64(whole), int64(fraction*1e9))` for a float interval, after
    /// `math.Modf`.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "Go's float to int64 conversions"
    )]
    pub fn from_interval(seconds: f64) -> Self {
        let whole = seconds.trunc();
        let fraction = seconds - whole;
        Self::unix(whole as i64, (fraction * 1e9) as i64)
    }

    /// Calendar fields in UTC: (year, month 1-12, day, hour, minute, second, weekday 0-6 from
    /// Sunday, day of year 1-366).
    #[must_use]
    pub fn civil(self) -> Civil {
        let days = self.seconds.div_euclid(86_400);
        let rest = self.seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let weekday = u32::try_from((days + 4).rem_euclid(7)).unwrap_or_default();
        let year_day = u32::try_from(days - days_from_civil(year, 1, 1) + 1).unwrap_or_default();
        Civil {
            year,
            month,
            day,
            hour: u32::try_from(rest / 3600).unwrap_or_default(),
            minute: u32::try_from(rest / 60 % 60).unwrap_or_default(),
            second: u32::try_from(rest % 60).unwrap_or_default(),
            weekday,
            year_day,
        }
    }

    /// Go's `time.Date(year, month, day, hour, minute, second, 0, time.UTC)`, which
    /// normalizes out-of-range fields.
    #[must_use]
    pub fn from_civil(
        year: i64,
        month: i64,
        day: i64,
        hour: i64,
        minute: i64,
        second: i64,
    ) -> Self {
        let month0 = month - 1;
        let year = year + month0.div_euclid(12);
        let month = month0.rem_euclid(12) + 1;
        let days = days_from_civil(year, u32::try_from(month).unwrap_or(1), 1) + day - 1;
        Self::unix(days * 86_400 + hour * 3600 + minute * 60 + second, 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub weekday: u32,
    pub year_day: u32,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

impl Civil {
    /// Go's `Format("Mon, 02 Jan 2006 15:04:05 GMT")`.
    #[must_use]
    pub fn http_date(&self) -> String {
        format!(
            "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
            WEEKDAYS[self.weekday as usize % 7],
            self.day,
            MONTHS[(self.month as usize + 11) % 12],
            go_year(self.year),
            self.hour,
            self.minute,
            self.second
        )
    }
}

/// Go's four-digit `2006` layout element.
fn go_year(year: i64) -> String {
    if year < 0 {
        format!("-{:04}", -year)
    } else {
        format!("{year:04}")
    }
}

/// Go's `time.Parse("Mon, 02 Jan 2006 15:04:05 GMT", value)`: names match ASCII
/// case-insensitively, the hour may have one digit, a fractional second is accepted after the
/// seconds, and the weekday is not checked against the date.
#[must_use]
pub fn parse_http_date(value: &str) -> Option<Date> {
    let mut rest = value.as_bytes();
    let name = |rest: &mut &[u8], names: &[&str]| -> Option<usize> {
        let found = names
            .iter()
            .position(|name| rest.len() >= 3 && rest[..3].eq_ignore_ascii_case(name.as_bytes()))?;
        *rest = &rest[3..];
        Some(found)
    };
    let literal = |rest: &mut &[u8], text: &str| -> Option<()> {
        *rest = rest.strip_prefix(text.as_bytes())?;
        Some(())
    };
    let number = |rest: &mut &[u8], min: usize, max: usize| -> Option<i64> {
        let digits = rest
            .iter()
            .take(max)
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits < min {
            return None;
        }
        let parsed = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
        *rest = &rest[digits..];
        Some(parsed)
    };
    name(&mut rest, &WEEKDAYS)?;
    literal(&mut rest, ", ")?;
    let day = number(&mut rest, 2, 2)?;
    literal(&mut rest, " ")?;
    let month = i64::try_from(name(&mut rest, &MONTHS)? + 1).ok()?;
    literal(&mut rest, " ")?;
    let year = number(&mut rest, 4, 4)?;
    literal(&mut rest, " ")?;
    let hour = number(&mut rest, 1, 2)?;
    literal(&mut rest, ":")?;
    let minute = number(&mut rest, 2, 2)?;
    literal(&mut rest, ":")?;
    let second = number(&mut rest, 2, 2)?;
    let mut nanos = 0i64;
    if rest.len() >= 2 && matches!(rest[0], b'.' | b',') && rest[1].is_ascii_digit() {
        let digits = rest[1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let fraction = &rest[1..=digits];
        for index in 0..9 {
            nanos = nanos * 10
                + fraction
                    .get(index)
                    .map_or(0, |digit| i64::from(digit - b'0'));
        }
        rest = &rest[1 + digits..];
    }
    literal(&mut rest, " GMT")?;
    if !rest.is_empty() || hour > 23 || minute > 59 || second > 59 || day < 1 {
        return None;
    }
    let next = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let days_in_month = days_from_civil(next.0, u32::try_from(next.1).ok()?, 1)
        - days_from_civil(year, u32::try_from(month).ok()?, 1);
    if day > days_in_month {
        return None;
    }
    let date = Date::from_civil(year, month, day, hour, minute, second);
    Some(Date::unix(date.seconds, nanos))
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// A guest object's host value: Go's `Object.Value`, whose dynamic type the bridge switches on.
#[derive(Clone, Default)]
pub enum Payload {
    #[default]
    Nil,
    String(String),
    Int(i64),
    Bool(bool),
    Float(f64),
    Data(Vec<u8>),
    Array(Vec<u64>),
    Dict(BTreeMap<String, u64>),
    Date(Date),
    /// `NSIndexSet`'s `[2]uint64{location, length}`.
    IndexSet([u64; 2]),
    /// Host values the bridge only passes around (cookies, URLs, HTTP bodies).
    Host(Rc<RefCell<dyn Any>>),
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.sprint())
    }
}

impl Payload {
    /// Go's `%T` of the value, for type-assertion failures.
    #[must_use]
    pub fn go_type(&self) -> &'static str {
        match self {
            Self::Nil => "nil",
            Self::String(_) => "string",
            Self::Int(_) => "int64",
            Self::Bool(_) => "bool",
            Self::Float(_) => "float64",
            Self::Data(_) => "[]uint8",
            Self::Array(_) => "[]uint64",
            Self::Dict(_) => "map[string]uint64",
            Self::Date(_) => "time.Time",
            Self::IndexSet(_) => "[2]uint64",
            Self::Host(_) => "host value",
        }
    }

    /// Go's `fmt.Sprint(value)`. Dates print in UTC.
    #[must_use]
    pub fn sprint(&self) -> String {
        match self {
            Self::Nil => "<nil>".to_owned(),
            Self::String(text) => text.clone(),
            Self::Int(value) => value.to_string(),
            Self::Bool(value) => value.to_string(),
            Self::Float(value) => go_float(*value),
            Self::Data(bytes) => go_list(bytes.iter()),
            Self::Array(items) => go_list(items.iter()),
            Self::IndexSet(items) => go_list(items.iter()),
            Self::Dict(map) => {
                let mut text = String::from("map[");
                for (index, (key, value)) in map.iter().enumerate() {
                    if index != 0 {
                        text.push(' ');
                    }
                    let _ = write!(text, "{key}:{value}");
                }
                text.push(']');
                text
            }
            Self::Date(date) => go_time(*date),
            Self::Host(_) => "<host value>".to_owned(),
        }
    }
}

fn go_list<T: std::fmt::Display>(items: impl Iterator<Item = T>) -> String {
    let parts: Vec<String> = items.map(|item| item.to_string()).collect();
    format!("[{}]", parts.join(" "))
}

/// Go's `time.Time.String` for a UTC time.
fn go_time(date: Date) -> String {
    let civil = date.civil();
    let mut text = format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        go_year(civil.year),
        civil.month,
        civil.day,
        civil.hour,
        civil.minute,
        civil.second
    );
    if date.nanos != 0 {
        let fraction = format!("{:09}", date.nanos);
        let _ = write!(text, ".{}", fraction.trim_end_matches('0'));
    }
    text.push_str(" +0000 UTC");
    text
}

/// Go's `%v` for a `float64`: shortest representation, exponent form below 1e-4 or from 1e21.
#[must_use]
pub fn go_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "+Inf" } else { "-Inf" }.to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or_default();
    if (-4..21).contains(&exponent) {
        return format!("{value}");
    }
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs())
}

/// A host value crossing the bridge: what Go's `Export` returns and `importValue` accepts.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Nil,
    String(String),
    Integer(i64),
    Real(f64),
    Boolean(bool),
    Data(Vec<u8>),
    Date(Date),
    Array(Vec<Value>),
    Dictionary(BTreeMap<String, Value>),
    /// A guest value with no Foundation equivalent, passed through by Go's `Export` (its Go
    /// type name).
    Other(&'static str),
}

impl Value {
    /// Go's `%T` of the value Go's `Export` would return.
    #[must_use]
    pub fn go_type(&self) -> &'static str {
        match self {
            Self::Nil => "<nil>",
            Self::String(_) => "string",
            Self::Integer(_) => "int64",
            Self::Real(_) => "float64",
            Self::Boolean(_) => "bool",
            Self::Data(_) => "[]uint8",
            Self::Date(_) => "time.Time",
            Self::Array(_) => "[]interface {}",
            Self::Dictionary(_) => "map[string]interface {}",
            Self::Other(kind) => kind,
        }
    }

    /// Go's `fmt.Sprint` of the value.
    #[must_use]
    pub fn sprint(&self) -> String {
        match self {
            Self::Nil => "<nil>".to_owned(),
            Self::String(text) => text.clone(),
            Self::Integer(value) => value.to_string(),
            Self::Real(value) => go_float(*value),
            Self::Boolean(value) => value.to_string(),
            Self::Data(bytes) => go_list(bytes.iter()),
            Self::Date(date) => go_time(*date),
            Self::Array(items) => format!(
                "[{}]",
                items.iter().map(Self::sprint).collect::<Vec<_>>().join(" ")
            ),
            Self::Dictionary(map) => format!(
                "map[{}]",
                map.iter()
                    .map(|(key, value)| format!("{key}:{}", value.sprint()))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Self::Other(kind) => format!("<{kind}>"),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_go() {
        assert_eq!(go_float(1.0), "1");
        assert_eq!(go_float(0.5), "0.5");
        assert_eq!(go_float(1e21), "1e+21");
        assert_eq!(go_float(1e20), "100000000000000000000");
        assert_eq!(go_float(0.0001), "0.0001");
        assert_eq!(go_float(0.000_015), "1.5e-05");
        assert_eq!(go_float(-2.5e-7), "-2.5e-07");
        let mut map = BTreeMap::new();
        map.insert("b".to_owned(), 2);
        map.insert("a".to_owned(), 1);
        assert_eq!(Payload::Dict(map).sprint(), "map[a:1 b:2]");
        assert_eq!(Payload::Data(vec![1, 2]).sprint(), "[1 2]");
        assert_eq!(Payload::Nil.sprint(), "<nil>");
        assert_eq!(
            Payload::Date(Date::unix(0, 500_000_000)).sprint(),
            "1970-01-01 00:00:00.5 +0000 UTC"
        );
    }

    #[test]
    fn converts_calendar_dates() {
        let date = Date::from_civil(2026, 11, 20, 19, 57, 59);
        assert_eq!(date.seconds, 1_795_204_679);
        let civil = date.civil();
        assert_eq!(
            (
                civil.year,
                civil.month,
                civil.day,
                civil.weekday,
                civil.year_day
            ),
            (2026, 11, 20, 5, 324)
        );
        assert_eq!(civil.http_date(), "Fri, 20 Nov 2026 19:57:59 GMT");
        assert_eq!(parse_http_date("Fri, 20 Nov 2026 19:57:59 GMT"), Some(date));
        assert_eq!(parse_http_date("Fri, 31 Nov 2026 19:57:59 GMT"), None);
        assert_eq!(parse_http_date("Xyz, 20 Nov 2026 19:57:59 GMT"), None);
        assert_eq!(
            parse_http_date("fri, 20 NOV 2026 9:57:59 GMT"),
            Some(date.add_seconds(-36_000))
        );
        assert_eq!(
            parse_http_date("Fri, 20 Nov 2026 19:57:59.5 GMT"),
            Some(Date::unix(date.seconds, 500_000_000))
        );
        assert_eq!(parse_http_date("Fri, 20 Nov 2026 19:57:59 GMTX"), None);
        assert_eq!(
            Date::from_civil(4001, 1, 1, 0, 0, 0).seconds,
            64_092_211_200
        );
        assert_eq!(Date::from_civil(1, 1, 1, 0, 0, 0).seconds, -62_135_596_800);
    }
}
