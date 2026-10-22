//! Shared JSON helpers that reproduce the JavaScript semantics the upstream pure
//! functions rely on: UTF-16 string lengths, `JSON.stringify`, truthiness, and the
//! leaf selector used by `buildSessionPath`.

use serde_json::Value;

/// Which entry `buildSessionPath` starts from. Pi's `leafId` is
/// `string | null | undefined`, and `null` means "no path" while `undefined`
/// means "the newest entry". JSON cannot express that distinction, so it is a
/// three-state enum here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafSelector {
    /// `undefined`: walk back from the last entry in the list.
    Default,
    /// `null`: no path at all.
    ExplicitNone,
    /// A concrete entry id; if it is not found the last entry is used, matching
    /// `leaf ??= entries[entries.length - 1]`.
    Id(String),
}

impl LeafSelector {
    /// `None` → [`LeafSelector::Default`], `Some(None)` → explicit none,
    /// `Some(Some(id))` → that id. Convenience for fixtures.
    pub fn from_optional(value: Option<Option<&str>>) -> Self {
        match value {
            None => LeafSelector::Default,
            Some(None) => LeafSelector::ExplicitNone,
            Some(Some(id)) => LeafSelector::Id(id.to_string()),
        }
    }
}

/// Compaction settings. Field names/order mirror `CompactionSettings`.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PureSettings {
    pub enabled: bool,
    pub reserve_tokens: i64,
    pub keep_recent_tokens: i64,
}

impl PureSettings {
    pub fn from_value(value: &Value) -> Self {
        PureSettings {
            enabled: bool_field(value, "enabled").unwrap_or(false),
            reserve_tokens: int_field(value, "reserveTokens").unwrap_or(0),
            keep_recent_tokens: int_field(value, "keepRecentTokens").unwrap_or(0),
        }
    }
}

/// `value[key]` as a string.
pub fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `value[key]` as an integer, accepting integral floats (JS numbers are all f64).
pub fn int_field(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(as_i64)
}

/// A JS number as `i64`, accepting integral `f64` values.
pub fn as_i64(value: &Value) -> Option<i64> {
    if let Some(i) = value.as_i64() {
        return Some(i);
    }
    value.as_f64().map(|f| f as i64)
}

/// A JS number as `f64`.
pub fn as_f64(value: &Value) -> Option<f64> {
    value.as_f64()
}

/// `value[key]` as a bool.
pub fn bool_field(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

/// `entry.type` (empty string when absent).
pub fn entry_type(value: &Value) -> &str {
    str_field(value, "type").unwrap_or("")
}

/// `entry.id`.
pub fn entry_id(value: &Value) -> Option<&str> {
    str_field(value, "id")
}

/// `entry.parentId`.
pub fn parent_id(value: &Value) -> Option<&str> {
    str_field(value, "parentId")
}

/// `message.role` (empty string when absent).
pub fn message_role(value: &Value) -> &str {
    str_field(value, "role").unwrap_or("")
}

/// JavaScript `string.length` — UTF-16 code units, not Unicode scalars or bytes.
pub fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// JavaScript truthiness for the values `block.text`/`message.content` can hold.
pub fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Serialize a value exactly like JavaScript's `JSON.stringify`.
///
/// Only the shapes produced by session parsing are supported: strings, finite
/// numbers, booleans, null, arrays and objects. Object keys keep their insertion
/// order (the workspace enables `serde_json/preserve_order`). Integral floats are
/// rendered without a trailing `.0`, matching JS (`JSON.stringify(1.0) === "1"`).
/// Non-finite numbers render as `null` (JS renders `NaN`/`Infinity` as `null`).
pub fn js_json_stringify(value: &Value) -> String {
    let mut out = String::new();
    write_json(value, &mut out);
    out
}

fn write_json(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(i) = number.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = number.as_u64() {
                out.push_str(&u.to_string());
            } else if let Some(f) = number.as_f64() {
                if f.is_finite() {
                    if f.fract() == 0.0 && f.abs() < 1e21 {
                        out.push_str(&(f as i64).to_string());
                    } else {
                        out.push_str(&format_js_float(f));
                    }
                } else {
                    out.push_str("null");
                }
            } else {
                out.push_str("null");
            }
        }
        Value::String(text) => write_json_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_json(item, out);
            }
            out.push('}');
        }
    }
}

/// Shortest round-trip float formatting close to JS `Number.prototype.toString`.
/// Exponents use JS's `e+21` shape rather than Rust's `e21`.
fn format_js_float(f: f64) -> String {
    let rendered = format!("{f}");
    if let Some((mantissa, exponent)) = rendered.split_once('e') {
        if !exponent.starts_with('-') && !exponent.starts_with('+') {
            return format!("{mantissa}e+{exponent}");
        }
    }
    rendered
}

fn write_json_string(text: &str, out: &mut String) {
    out.push('"');
    for unit in text.encode_utf16() {
        match unit {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            u if u < 0x20 => out.push_str(&format!("\\u{u:04x}")),
            u => out.push(char::from_u32(u as u32).unwrap_or('\u{fffd}')),
        }
    }
    out.push('"');
}

/// Recursively compare JSON values by value. Numbers compare as `f64`, which is
/// the contract's single allowed normalization (JS has one number type).
pub fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(x, y)| json_eq(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, xv)| y.get(key).map(|yv| json_eq(xv, yv)).unwrap_or(false))
        }
        _ => false,
    }
}
