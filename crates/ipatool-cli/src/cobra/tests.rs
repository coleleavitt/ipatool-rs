//! Expected texts were recorded from the Go binary (upstream `3b02b4c` with the fork's
//! `list-versions` flags) for the same command lines.

use clap::{CommandFactory as _, Parser as _};

use super::*;
use crate::{AuthCommand, Cli, Command};

fn go(args: &[&str]) -> Usage {
    let tokens: Vec<String> = args.iter().map(|&arg| arg.to_owned()).collect();
    let error = Cli::try_parse_from(std::iter::once("ipatool").chain(args.iter().copied()))
        .err()
        .expect("clap rejects the command line");
    usage(&error, &tokens, &Cli::command()).expect("Go reports it")
}

fn error(message: &str, json: bool) -> Usage {
    Usage::Error {
        message: message.to_owned(),
        json,
    }
}

fn parses(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("ipatool").chain(args.iter().copied()))
        .unwrap_or_else(|error| panic!("{args:?}: {error}"))
}

#[test]
fn unknown_commands_suggest_like_cobra() {
    for (args, message) in [
        (
            &["serch", "x"][..],
            "unknown command \"serch\" for \"ipatool\"\n\nDid you mean this?\n\tsearch\n",
        ),
        (
            &["down"],
            "unknown command \"down\" for \"ipatool\"\n\nDid you mean this?\n\tdownload\n",
        ),
        (
            &["list"],
            "unknown command \"list\" for \"ipatool\"\n\nDid you mean this?\n\tlist-purchases\n\tlist-versions\n",
        ),
        (&["bogus"], "unknown command \"bogus\" for \"ipatool\""),
        // Cobra finds the command before it parses flags, so the line is text.
        (
            &["--format", "json", "bogus"],
            "unknown command \"bogus\" for \"ipatool\"",
        ),
    ] {
        assert_eq!(go(args), error(message, false), "{args:?}");
    }
}

#[test]
fn command_groups_print_their_help() {
    for args in [&[][..], &["auth"], &["auth", "bogus"]] {
        let Usage::Help(help) = go(args) else {
            panic!("{args:?} is not help");
        };
        assert!(help.contains("Usage: ipatool"), "{args:?}: {help}");
    }
    let Usage::Help(help) = go(&["auth", "infoo"]) else {
        panic!("not help");
    };
    assert!(help.contains("ipatool auth"), "{help}");
}

#[test]
fn unknown_flags_use_pflag_texts() {
    for (args, message, json) in [
        (
            &["search", "--bogus", "x"][..],
            "unknown flag: --bogus",
            false,
        ),
        (
            &["search", "--bogus=1", "x"],
            "unknown flag: --bogus",
            false,
        ),
        (
            &["search", "-z", "x"],
            "unknown shorthand flag: 'z' in -z",
            false,
        ),
        // The format is honored when pflag parsed it before the bad flag.
        (
            &["search", "x", "--format=json", "--bogus"],
            "unknown flag: --bogus",
            true,
        ),
        (
            &["search", "--bogus", "x", "--format", "json"],
            "unknown flag: --bogus",
            false,
        ),
    ] {
        assert_eq!(go(args), error(message, json), "{args:?}");
    }
}

#[test]
fn no_args_commands_reject_arguments_after_parsing_flags() {
    assert_eq!(
        go(&["list-purchases", "extra", "--format", "json"]),
        error(
            "unknown command \"extra\" for \"ipatool list-purchases\"",
            true
        )
    );
    assert_eq!(
        go(&["mcp", "extra"]),
        error("unknown command \"extra\" for \"ipatool mcp\"", false)
    );
}

#[test]
fn bad_values_use_pflag_texts() {
    for (args, message, json) in [
        (
            &["search", "x", "--limit", "abc", "--format", "json"][..],
            r#"invalid argument "abc" for "-l, --limit" flag: strconv.ParseInt: parsing "abc": invalid syntax"#,
            false,
        ),
        (
            &["--format", "json", "download", "-i", "abc"],
            r#"invalid argument "abc" for "-i, --app-id" flag: strconv.ParseInt: parsing "abc": invalid syntax"#,
            true,
        ),
        (
            &["list-versions", "-i", "1", "--concurrency", "abc"],
            r#"invalid argument "abc" for "--concurrency" flag: strconv.ParseInt: parsing "abc": invalid syntax"#,
            false,
        ),
        (
            &["list-purchases", "-p", "9223372036854775808"],
            r#"invalid argument "9223372036854775808" for "-p, --page" flag: strconv.ParseInt: parsing "9223372036854775808": value out of range"#,
            false,
        ),
        (
            &[
                "download",
                "-i",
                "1",
                "--purchase=maybe",
                "--format",
                "json",
            ],
            r#"invalid argument "maybe" for "--purchase" flag: strconv.ParseBool: parsing "maybe": invalid syntax"#,
            false,
        ),
        (
            &["search", "x", "--verbose=maybe"],
            r#"invalid argument "maybe" for "--verbose" flag: strconv.ParseBool: parsing "maybe": invalid syntax"#,
            false,
        ),
        (
            &["--format", "xml", "search", "x"],
            r#"invalid argument "xml" for "--format" flag: must be 'json', 'text'"#,
            false,
        ),
        (
            &["search", "x", "--limit"],
            "flag needs an argument: --limit",
            false,
        ),
        (
            &["search", "x", "-l"],
            "flag needs an argument: 'l' in -l",
            false,
        ),
        (&["--format"], "flag needs an argument: --format", false),
        (
            &[
                "--format",
                "json",
                "get-version-metadata",
                "-i",
                "1",
                "--external-version-id",
            ],
            "flag needs an argument: --external-version-id",
            true,
        ),
    ] {
        assert_eq!(go(args), error(message, json), "{args:?}");
    }
}

#[test]
fn accepts_what_pflag_accepts() {
    let Cli {
        command: Command::Search { args, limit, .. },
        verbose,
        ..
    } = parses(&["search", "-l", "-1", "--verbose=false", "x", "y"])
    else {
        panic!("not search");
    };
    assert_eq!(
        (args.as_slice(), limit, verbose),
        (&["x".to_owned(), "y".to_owned()][..], -1, false)
    );
    assert!(matches!(
        parses(&["auth", "info", "extra"]).command,
        Command::Auth(AuthCommand::Info(_))
    ));
    assert!(matches!(
        parses(&["download", "-i", "1", "extra", "--purchase=f"]).command,
        Command::Download(_)
    ));
    assert!(parses(&["--non-interactive=T", "mcp"]).non_interactive);
}

#[test]
fn parses_integers_like_strconv() {
    for (value, parsed) in [
        ("0x10", 16),
        ("0X1F", 31),
        ("010", 8),
        ("0o17", 15),
        ("0b101", 5),
        ("1_000", 1000),
        ("0x_1", 1),
        ("0_1", 1),
        ("+5", 5),
        ("-5", -5),
        ("-9223372036854775808", i64::MIN),
        ("-0x8000000000000000", i64::MIN),
    ] {
        assert_eq!(parse_int(value), Ok(parsed), "{value}");
    }
    for (value, reason) in [
        ("abc", "invalid syntax"),
        ("", "invalid syntax"),
        ("_1", "invalid syntax"),
        ("1_", "invalid syntax"),
        ("1__0", "invalid syntax"),
        ("08", "invalid syntax"),
        ("--1", "invalid syntax"),
        ("\u{663}", "invalid syntax"),
        ("9223372036854775808", "value out of range"),
        ("-9223372036854775809", "value out of range"),
    ] {
        assert_eq!(
            parse_int(value),
            Err(format!(
                "strconv.ParseInt: parsing {}: {reason}",
                go_quote(value.as_bytes())
            )),
            "{value}"
        );
    }
}

#[test]
fn parses_booleans_like_strconv() {
    for value in ["1", "t", "T", "TRUE", "true", "True"] {
        assert_eq!(parse_bool(value), Ok(true));
    }
    for value in ["0", "f", "F", "FALSE", "false", "False"] {
        assert_eq!(parse_bool(value), Ok(false));
    }
    assert_eq!(
        parse_bool("yes"),
        Err(r#"strconv.ParseBool: parsing "yes": invalid syntax"#.to_owned())
    );
}
