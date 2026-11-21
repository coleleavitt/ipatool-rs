//! The parts of Go's MCP SDK (`go-sdk` v1.8.0) and `jsonschema-go` v0.4.3 that the
//! tools depend on: the schema shape `jsonschema.For` infers, the argument validation
//! of `mcp.AddTool`, and the JSON text of a structured result.

use rmcp::model::JsonObject;
use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema};
use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value};

/// The schema of a tool's arguments.
pub(super) fn input_schema<T: JsonSchema>() -> JsonObject {
    go_schema::<T>(SchemaSettings::draft2020_12().for_deserialize())
}

/// The schema of a tool's structured result.
pub(super) fn output_schema<T: JsonSchema>() -> JsonObject {
    go_schema::<T>(SchemaSettings::draft2020_12().for_serialize())
}

/// `jsonschema.For` writes every type inline, with no titles, formats or bounds, and
/// lists `null` first for Go pointers and slices.
fn go_schema<T: JsonSchema>(settings: SchemaSettings) -> JsonObject {
    let schema: Schema = settings
        .with(|settings| {
            settings.inline_subschemas = true;
            settings.meta_schema = None;
        })
        .into_generator()
        .into_root_schema_for::<T>();
    let mut value = schema.to_value();
    go_shape(&mut value);
    match value {
        Value::Object(object) => object,
        _ => JsonObject::new(),
    }
}

fn go_shape(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for key in ["title", "format", "minimum", "default"] {
                object.remove(key);
            }
            if let Some(Value::Array(types)) = object.get_mut("type") {
                types.sort_by_key(|kind| kind != "null");
            }
            object.values_mut().for_each(go_shape);
        }
        Value::Array(items) => items.iter_mut().for_each(go_shape),
        _ => {}
    }
}

/// `mcp.AddTool`'s handling of `arguments`: absent or `null` arguments are an empty
/// object, which is validated against the input schema before it is decoded.
pub(super) fn parse_arguments<T: DeserializeOwned>(
    schema: &JsonObject,
    arguments: Option<JsonObject>,
) -> Result<T, String> {
    let arguments = Value::Object(arguments.unwrap_or_default());
    validate(&arguments, &Value::Object(schema.clone()), "root")
        .map_err(|error| format!("validating \"arguments\": {error}"))?;
    // Go decodes the validated arguments again from JSON, where a whole number such
    // as `5.0` is written as `5`.
    serde_json::from_value(whole_numbers(arguments)).map_err(|error| error.to_string())
}

/// `jsonschema.Resolved.Validate` for the keywords the tool schemas use: `type`,
/// `properties`, `additionalProperties: false` and `required`, checked in that order.
fn validate(instance: &Value, schema: &Value, location: &str) -> Result<(), String> {
    validate_keywords(instance, schema).map_err(|error| format!("validating {location}: {error}"))
}

fn validate_keywords(instance: &Value, schema: &Value) -> Result<(), String> {
    let got = json_type(instance);
    match schema.get("type") {
        Some(Value::String(want)) if !type_matches(got, want) => {
            return Err(format!(
                "type: {} has type {got:?}, want {want:?}",
                go_format(instance, true)
            ));
        }
        Some(Value::Array(types)) => {
            let types: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
            if !types.iter().any(|want| type_matches(got, want)) {
                return Err(format!(
                    "type: {} has type {got:?}, want one of {:?}",
                    go_format(instance, true),
                    types.join(", ")
                ));
            }
        }
        _ => {}
    }
    let Value::Object(object) = instance else {
        return Ok(());
    };
    let properties = schema.get("properties").and_then(Value::as_object);
    for (name, subschema) in properties.into_iter().flatten() {
        if let Some(value) = object.get(name) {
            validate(value, subschema, &format!("/properties/{name}"))?;
        }
    }
    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        let unexpected: Vec<&String> = object
            .keys()
            .filter(|name| !properties.is_some_and(|properties| properties.contains_key(*name)))
            .collect();
        if !unexpected.is_empty() {
            return Err(format!(
                "unexpected additional properties {}",
                go_quoted_list(&unexpected)
            ));
        }
    }
    let required = schema.get("required").and_then(Value::as_array);
    let missing: Vec<&String> = required
        .into_iter()
        .flatten()
        .filter_map(|name| match name {
            Value::String(name) if !object.contains_key(name) => Some(name),
            _ => None,
        })
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "required: missing properties: {}",
            go_quoted_list(&missing)
        ));
    }
    Ok(())
}

/// "number" accepts integers, as in JSON Schema.
fn type_matches(got: &str, want: &str) -> bool {
    got == want || (got == "integer" && want == "number")
}

/// `jsonschema`'s `jsonType`: Go decodes every number as a float64, and a whole one
/// is an integer.
fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.as_f64().is_some_and(|float| float.fract() == 0.0) => {
            "integer"
        }
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Go's `%q` of a `[]string`: `["a" "b"]`.
fn go_quoted_list(items: &[&String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("{item:?}")).collect();
    format!("[{}]", quoted.join(" "))
}

/// Go's `%v` of a decoded JSON value (a `reflect.Value` at the top, where `null` is the
/// invalid value).
fn go_format(value: &Value, top: bool) -> String {
    match value {
        Value::Null if top => "<invalid reflect.Value>".to_owned(),
        Value::Null => "<nil>".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number.as_f64().map_or_else(String::new, go_float),
        Value::String(value) => value.clone(),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(|item| go_format(item, false)).collect();
            format!("[{}]", items.join(" "))
        }
        Value::Object(object) => {
            let mut entries: Vec<(&String, &Value)> = object.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            let entries: Vec<String> = entries
                .into_iter()
                .map(|(key, value)| format!("{key}:{}", go_format(value, false)))
                .collect();
            format!("map[{}]", entries.join(" "))
        }
    }
}

/// Go's `%v` of a float64: the shortest digits, in exponent form below 1e-4 and from
/// 1e21.
fn go_float(value: f64) -> String {
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (sign, mantissa) = mantissa
        .strip_prefix('-')
        .map_or(("", mantissa), |rest| ("-", rest));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    if !(-4..21).contains(&exponent) {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        return format!(
            "{sign}{first}{fraction}e{exponent_sign}{:02}",
            exponent.abs()
        );
    }
    let point = exponent + 1;
    let mut text = String::from(sign);
    if point <= 0 {
        text.push_str("0.");
        text.extend(std::iter::repeat_n('0', point.unsigned_abs() as usize));
        text.push_str(&digits);
    } else {
        let point = point.unsigned_abs() as usize;
        if digits.len() <= point {
            text.push_str(&digits);
            text.extend(std::iter::repeat_n('0', point - digits.len()));
        } else {
            text.push_str(&digits[..point]);
            text.push('.');
            text.push_str(&digits[point..]);
        }
    }
    text
}

/// Whole floats become integers, as Go writes them back to JSON before decoding the
/// tool input or after encoding a result through `any`.
fn whole_numbers(value: Value) -> Value {
    match value {
        Value::Number(number) => Value::Number(whole_number(number)),
        Value::Array(items) => Value::Array(items.into_iter().map(whole_numbers).collect()),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, whole_numbers(value)))
                .collect(),
        ),
        value => value,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "only whole values in i64 range are converted"
)]
fn whole_number(number: Number) -> Number {
    match number.as_f64() {
        Some(float)
            if !number.is_i64()
                && !number.is_u64()
                && float.fract() == 0.0
                && float.abs() < 9.2e18 =>
        {
            Number::from(float as i64)
        }
        _ => number,
    }
}

/// A structured result as Go's SDK sends it: decoded into `any` (object keys sorted,
/// whole floats written as integers).
pub(super) fn go_value(value: Value) -> Value {
    sorted(whole_numbers(value))
}

fn sorted(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        Value::Object(object) => {
            let mut entries: Vec<(String, Value)> = object.into_iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sorted(value)))
                    .collect::<Map<String, Value>>(),
            )
        }
        value => value,
    }
}

/// `encoding/json`'s text, which escapes `<`, `>`, `&`, U+2028 and U+2029 in strings.
pub(super) fn go_json_text(value: &Value) -> String {
    let mut text = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut text, GoFormatter);
    match serde::Serialize::serialize(value, &mut serializer) {
        Ok(()) => String::from_utf8(text).unwrap_or_default(),
        Err(_) => value.to_string(),
    }
}

struct GoFormatter;

impl serde_json::ser::Formatter for GoFormatter {
    fn write_string_fragment<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> std::io::Result<()> {
        let mut start = 0;
        for (index, character) in fragment.char_indices() {
            if matches!(character, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') {
                writer.write_all(&fragment.as_bytes()[start..index])?;
                write!(writer, "\\u{:04x}", u32::from(character))?;
                start = index + character.len_utf8();
            }
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}

#[cfg(test)]
mod tests {
    use super::go_float;

    #[test]
    fn formats_floats_as_go_percent_v() {
        let cases = [
            (123.0, "123"),
            (1.5, "1.5"),
            (-5.0, "-5"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1e20, "100000000000000000000"),
            (-1e21, "-1e+21"),
            (-9_223_372_036_854_775_809.0, "-9223372036854776000"),
            (1.25e-7, "1.25e-07"),
        ];
        for (value, expected) in cases {
            assert_eq!(go_float(value), expected, "{value}");
        }
    }
}
