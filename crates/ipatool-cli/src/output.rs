//! Log-style output matching the Go CLI: JSON mode prints one zerolog-shaped
//! object per line (`level`, fields, `time`, `message`); text mode prints the level,
//! the message and `key=value` pairs.

use clap::ValueEnum;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Text,
    Json,
}

pub struct Output {
    format: Format,
    verbose: bool,
}

impl Output {
    pub fn new(format: Format, verbose: bool) -> Self {
        Self { format, verbose }
    }

    /// `Logger.Log()...Send()`.
    pub fn info(&self, fields: Value) {
        println!("{}", self.render("info", fields, None));
    }

    /// `Logger.Log().Msg(message)`.
    pub fn message(&self, message: &str) {
        println!("{}", self.render("info", Value::Null, Some(message)));
    }

    /// `Logger.Verbose()...Msg(message)`: printed only with `--verbose`.
    pub fn verbose(&self, fields: Value, message: &str) {
        if self.verbose {
            println!("{}", self.render("debug", fields, Some(message)));
        }
    }

    pub fn error(&self, error: &dyn std::error::Error) {
        let line = self.render(
            "error",
            serde_json::json!({ "error": error.to_string() }),
            None,
        );
        match self.format {
            Format::Json => println!("{line}"),
            Format::Text => eprintln!("{line}"),
        }
    }

    fn render(&self, level: &str, fields: Value, message: Option<&str>) -> String {
        let fields = match fields {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            other => Map::from_iter([("message".to_owned(), other)]),
        };
        match self.format {
            Format::Json => {
                let mut line = Map::new();
                line.insert("level".into(), level.into());
                line.extend(fields);
                // zerolog's default time format is RFC 3339 in whole seconds.
                let now = OffsetDateTime::now_local()
                    .unwrap_or_else(|_| OffsetDateTime::now_utc())
                    .truncate_to_second();
                line.insert(
                    "time".into(),
                    now.format(&Rfc3339).unwrap_or_default().into(),
                );
                if let Some(message) = message {
                    line.insert("message".into(), message.into());
                }
                Value::Object(line).to_string()
            }
            Format::Text => {
                let level = match level {
                    "error" => "ERR",
                    "debug" => "DBG",
                    _ => "INF",
                };
                let mut parts = vec![level.to_owned()];
                parts.extend(message.map(str::to_owned));
                parts.extend(fields.into_iter().map(|(key, value)| match value {
                    Value::String(s) => format!("{key}={s}"),
                    other => format!("{key}={other}"),
                }));
                parts.join(" ")
            }
        }
    }
}
