//! Go's usage errors. Cobra and pflag report a bad command line as one error line in the
//! log format with exit status 1, and print a command group's help when it gets no or an
//! unknown subcommand. clap's errors are turned into the same texts here.

use clap::error::{ContextKind, ContextValue, ErrorKind};
use ipatool_store::gourl::go_quote;

/// Cobra's default for a command without subcommands and without an `Args` check: extra
/// positional arguments are accepted and ignored.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct IgnoredArgs {
    #[arg(hide = true)]
    _args: Vec<String>,
}

/// What Go does instead of clap's own error output.
#[derive(Debug, PartialEq, Eq)]
pub enum Usage {
    /// Cobra prints the help of a command group on stdout and exits 0.
    Help(String),
    /// One error line. `json` when `--format json` was parsed before pflag stopped.
    Error { message: String, json: bool },
}

/// The Go equivalent of `error`, or `None` to let clap report it (help, version, and the
/// port's own flags that have no Go counterpart).
pub fn usage(error: &clap::Error, argv: &[String], command: &clap::Command) -> Option<Usage> {
    let path = command_path(error);
    match error.kind() {
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            Some(Usage::Help(error.render().to_string()))
        }
        ErrorKind::InvalidSubcommand => unknown_command(
            command,
            &path,
            context(error, ContextKind::InvalidSubcommand)?,
        ),
        ErrorKind::UnknownArgument => {
            unknown_argument(argv, &path, context(error, ContextKind::InvalidArg)?)
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => invalid_value(error, argv, command),
        _ => None,
    }
}

fn context(error: &clap::Error, kind: ContextKind) -> Option<&str> {
    match error.get(kind) {
        Some(ContextValue::String(value)) => Some(value),
        _ => None,
    }
}

/// An unknown name where a subcommand belongs: an error at the root, the group's help below.
fn unknown_command(command: &clap::Command, path: &[String], name: &str) -> Option<Usage> {
    if path.len() > 1 {
        let help = command
            .clone()
            .try_get_matches_from(path.iter().map(String::as_str).chain(["--help"]))
            .err()?;
        return Some(Usage::Help(help.render().to_string()));
    }
    Some(Usage::Error {
        message: format!(
            "unknown command {} for \"ipatool\"{}",
            go_quote(name.as_bytes()),
            suggestions(command, name)
        ),
        json: false,
    })
}

fn unknown_argument(argv: &[String], path: &[String], token: &str) -> Option<Usage> {
    let parsed = &argv[..argv
        .iter()
        .position(|arg| arg == token)
        .unwrap_or(argv.len())];
    let message = if let Some(long) = token.strip_prefix("--") {
        let name = long.split_once('=').map_or(long, |(name, _)| name);
        format!("unknown flag: --{name}")
    } else if let Some(short) = token.strip_prefix('-').and_then(|s| s.chars().next()) {
        format!("unknown shorthand flag: '{short}' in {token}")
    } else if matches!(
        path.last().map(String::as_str),
        Some("list-purchases" | "mcp")
    ) {
        // Cobra's `NoArgs`, checked once every flag is parsed.
        return Some(Usage::Error {
            message: format!(
                "unknown command {} for {}",
                go_quote(token.as_bytes()),
                go_quote(path.join(" ").as_bytes())
            ),
            json: json_format(argv),
        });
    } else {
        return None;
    };
    Some(Usage::Error {
        message,
        json: json_format(parsed),
    })
}

/// pflag's errors for a missing or unparsable flag value.
fn invalid_value(error: &clap::Error, argv: &[String], command: &clap::Command) -> Option<Usage> {
    let long = context(error, ContextKind::InvalidArg)?
        .strip_prefix("--")?
        .split([' ', '['])
        .next()?;
    let value = context(error, ContextKind::InvalidValue).unwrap_or_default();
    let short = find_short(command, long);
    let index = argv.iter().position(|token| {
        token == &format!("--{long}")
            || token.starts_with(&format!("--{long}="))
            || short.is_some_and(|short| token.starts_with(&format!("-{short}")))
    });
    let token = index.map(|index| argv[index].as_str());
    let message = if error.kind() == ErrorKind::InvalidValue && value.is_empty() {
        match short {
            Some(short) if token == Some(format!("-{short}").as_str()) => {
                format!("flag needs an argument: '{short}' in -{short}")
            }
            _ => format!("flag needs an argument: --{long}"),
        }
    } else {
        let name = short.map_or(format!("--{long}"), |short| format!("-{short}, --{long}"));
        let reason = if long == "format" {
            "must be 'json', 'text'".to_owned()
        } else {
            std::error::Error::source(error)?.to_string()
        };
        format!(
            "invalid argument {} for {} flag: {reason}",
            go_quote(value.as_bytes()),
            go_quote(name.as_bytes())
        )
    };
    Some(Usage::Error {
        message,
        json: long != "format" && json_format(&argv[..index.unwrap_or(argv.len())]),
    })
}

/// The command names of the usage line, `ipatool` first.
fn command_path(error: &clap::Error) -> Vec<String> {
    let Some(ContextValue::StyledStr(usage)) = error.get(ContextKind::Usage) else {
        return vec!["ipatool".to_owned()];
    };
    usage
        .to_string()
        .trim_start_matches("Usage:")
        .split_whitespace()
        .take_while(|word| !word.starts_with(['-', '[', '<']))
        .map(str::to_owned)
        .collect()
}

/// A flag's shorthand; Go uses the same letter for a long name everywhere.
fn find_short(command: &clap::Command, long: &str) -> Option<char> {
    command
        .get_arguments()
        .find(|arg| arg.get_long() == Some(long))
        .and_then(clap::Arg::get_short)
        .or_else(|| {
            command
                .get_subcommands()
                .find_map(|command| find_short(command, long))
        })
}

/// The last `--format` value pflag had parsed is `json`.
fn json_format(argv: &[String]) -> bool {
    let mut json = false;
    let mut tokens = argv.iter();
    while let Some(arg) = tokens.next() {
        let value = match arg.strip_prefix("--format") {
            Some("") => tokens.next().map(String::as_str),
            Some(rest) => rest.strip_prefix('='),
            None => None,
        };
        if let Some(value) = value {
            json = value == "json";
        }
    }
    json
}

/// Cobra's `findSuggestions`: commands within Levenshtein distance 2 (ignoring case) or
/// starting with the typed name, in name order.
fn suggestions(command: &clap::Command, typed: &str) -> String {
    let typed = typed.to_lowercase();
    let mut names: Vec<&str> = command
        .get_subcommands()
        .filter(|command| !command.is_hide_set() && command.get_name() != "help")
        .map(clap::Command::get_name)
        .filter(|name| {
            let name = name.to_lowercase();
            levenshtein(&typed, &name) <= 2 || name.starts_with(&typed)
        })
        .collect();
    if names.is_empty() {
        return String::new();
    }
    names.sort_unstable();
    let mut out = "\n\nDid you mean this?\n".to_owned();
    for name in names {
        out.push('\t');
        out.push_str(name);
        out.push('\n');
    }
    out
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let substitution = previous + usize::from(ca != cb);
            previous = row[j + 1];
            row[j + 1] = substitution.min(previous + 1).min(row[j] + 1);
        }
    }
    row[b.len()]
}

/// pflag's integer flags: `strconv.ParseInt(value, 0, 64)`, so base prefixes, a leading
/// zero for octal and underscores between digits are accepted.
pub fn parse_int(value: &str) -> Result<i64, String> {
    let error = |reason: &str| {
        format!(
            "strconv.ParseInt: parsing {}: {reason}",
            go_quote(value.as_bytes())
        )
    };
    let (negative, unsigned) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let lower = unsigned.to_ascii_lowercase();
    let (radix, digits, prefixed) = match lower.get(..2) {
        Some("0x") => (16, &unsigned[2..], true),
        Some("0o") => (8, &unsigned[2..], true),
        Some("0b") => (2, &unsigned[2..], true),
        _ if unsigned.len() > 1 && unsigned.starts_with('0') => (8, &unsigned[1..], true),
        _ => (10, unsigned, false),
    };
    // Go allows an underscore only between digits, or after a base prefix.
    let underscores_ok =
        !digits.ends_with('_') && !digits.contains("__") && (prefixed || !digits.starts_with('_'));
    let digits = digits.replace('_', "");
    if digits.is_empty() || !underscores_ok || digits.starts_with(['+', '-']) {
        return Err(error("invalid syntax"));
    }
    let magnitude = u64::from_str_radix(&digits, radix).map_err(|err| {
        if matches!(err.kind(), std::num::IntErrorKind::PosOverflow) {
            error("value out of range")
        } else {
            error("invalid syntax")
        }
    })?;
    let parsed = if negative {
        0i64.checked_sub_unsigned(magnitude)
    } else {
        i64::try_from(magnitude).ok()
    };
    parsed.ok_or_else(|| error("value out of range"))
}

/// pflag's boolean flags: `strconv.ParseBool`.
pub fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(format!(
            "strconv.ParseBool: parsing {}: invalid syntax",
            go_quote(value.as_bytes())
        )),
    }
}

#[cfg(test)]
mod tests;
