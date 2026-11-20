use std::collections::BTreeMap;

use serde_json::Value as Json;

use super::*;

fn oracle() -> Json {
    serde_json::from_str(include_str!("../../../tests/fixtures/network-oracle.json"))
        .expect("the oracle fixture is valid JSON")
}

/// Rebuilds a value from the oracle's typed description.
fn value(typed: &Json) -> Value {
    match typed["t"].as_str().unwrap_or_default() {
        "nil" => Value::Nil,
        "string" => Value::String(typed["v"].as_str().unwrap_or_default().to_owned()),
        "int" => Value::Integer(typed["v"].as_i64().unwrap_or_default()),
        "real" => Value::Real(f64::from_bits(typed["v"].as_u64().unwrap_or_default())),
        "bool" => Value::Boolean(typed["v"].as_bool().unwrap_or_default()),
        "data" => Value::Data(
            (0..typed["v"].as_str().unwrap_or_default().len())
                .step_by(2)
                .map(|i| {
                    u8::from_str_radix(&typed["v"].as_str().unwrap_or_default()[i..i + 2], 16)
                        .unwrap_or_default()
                })
                .collect(),
        ),
        "date" => Value::Date(Date::unix(
            typed["s"].as_i64().unwrap_or_default(),
            typed["n"].as_i64().unwrap_or_default(),
        )),
        "array" => Value::Array(
            typed["v"]
                .as_array()
                .map_or_else(Vec::new, |items| items.iter().map(value).collect()),
        ),
        "dict" => Value::Dictionary(typed["v"].as_object().map_or_else(BTreeMap::new, |map| {
            map.iter().map(|(k, v)| (k.clone(), value(v))).collect()
        })),
        other => panic!("unknown oracle value type {other}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, b| {
        let _ = write!(text, "{b:02x}");
        text
    })
}

#[test]
fn encodes_like_go() {
    let oracle = oracle();
    let cases = oracle["codec"].as_array().expect("codec cases");
    assert!(cases.len() >= 10);
    for case in cases {
        let input = value(&case["Value"]);
        let text = |key: &str| case[key].as_str().unwrap_or_default().to_owned();
        match plist_xml(&input) {
            Ok(xml) => assert_eq!(String::from_utf8_lossy(&xml), text("XML"), "{input:?}"),
            Err(err) => assert_eq!(err.to_string(), text("XMLError"), "{input:?}"),
        }
        match plist_binary(&input) {
            Ok(binary) => assert_eq!(hex(&binary), text("Binary"), "{input:?}"),
            Err(err) => assert!(!text("BinaryError").is_empty(), "{input:?}: {err}"),
        }
        match json(&input) {
            Ok(json) => assert_eq!(String::from_utf8_lossy(&json), text("JSON"), "{input:?}"),
            Err(err) => assert_eq!(err.to_string(), text("JSONError"), "{input:?}"),
        }
    }
}

#[test]
fn decodes_like_go() {
    let oracle = oracle();
    for case in oracle["decode"].as_array().expect("decode cases") {
        let input = case["Input"].as_str().unwrap_or_default();
        let decoded = match case["Format"].as_str().unwrap_or_default() {
            "json" => parse_json(input.as_bytes()),
            "plist" => parse_plist(input.as_bytes()),
            _ => {
                let bytes: Vec<u8> = (0..input.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&input[i..i + 2], 16).unwrap_or_default())
                    .collect();
                parse_plist(&bytes)
            }
        };
        if case["Error"].as_bool().unwrap_or_default() {
            assert!(decoded.is_err(), "{input}");
        } else {
            assert_eq!(decoded.ok(), Some(value(&case["Value"])), "{input}");
        }
    }
}

#[test]
fn formats_floats_like_strconv() {
    for (value, want) in [
        (1e6, "1e+06"),
        (123_456.0, "123456"),
        (1e-5, "1e-05"),
        (-0.0, "-0"),
        (0.000_1, "0.0001"),
        (1e100, "1e+100"),
    ] {
        assert_eq!(format_g(value), want);
    }
}
