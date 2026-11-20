//! Log-style output matching the Go CLI: JSON mode prints one zerolog-shaped
//! object per line (`level`, fields, `time`); text mode prints `key=value` pairs.

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
    #[allow(dead_code, reason = "used once commands emit debug logs")]
    verbose: bool,
}

impl Output {
    pub fn new(format: Format, verbose: bool) -> Self {
        Self { format, verbose }
    }

    pub fn info(&self, fields: Value) {
        println!("{}", self.render("info", fields));
    }

    pub fn error(&self, error: &dyn std::error::Error) {
        let line = self.render("error", serde_json::json!({ "error": error.to_string() }));
        match self.format {
            Format::Json => println!("{line}"),
            Format::Text => eprintln!("{line}"),
        }
    }

    fn render(&self, level: &str, fields: Value) -> String {
        let fields = match fields {
            Value::Object(map) => map,
            other => Map::from_iter([("message".to_owned(), other)]),
        };
        match self.format {
            Format::Json => {
                let mut line = Map::new();
                line.insert("level".into(), level.into());
                line.extend(fields);
                let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
                line.insert(
                    "time".into(),
                    now.format(&Rfc3339).unwrap_or_default().into(),
                );
                Value::Object(line).to_string()
            }
            Format::Text => {
                let level = if level == "error" { "ERR" } else { "INF" };
                let pairs: Vec<String> = fields
                    .into_iter()
                    .map(|(key, value)| match value {
                        Value::String(s) => format!("{key}={s}"),
                        other => format!("{key}={other}"),
                    })
                    .collect();
                format!("{level} {}", pairs.join(" "))
            }
        }
    }
}
