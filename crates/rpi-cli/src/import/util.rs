//! Small helpers shared by the import categories: hashing, canonical JSON,
//! private atomic writes and tolerant JSON-object parsing.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Serialize a JSON value with every object's keys sorted, so the digest of a
/// value does not depend on the key order of the file it came from.
pub(crate) fn canonical_json(value: &Value) -> Vec<u8> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for key in keys {
                    out.insert(key.clone(), sorted(&map[key]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_vec(&sorted(value)).unwrap_or_default()
}

/// Digest of a JSON value in canonical form.
pub(crate) fn canonical_sha256(value: &Value) -> String {
    sha256_hex(&canonical_json(value))
}

/// Parse `bytes` as a JSON object, tolerating a leading UTF-8 BOM (Pi's own
/// reader strips one before parsing `auth.json`).
pub(crate) fn parse_object(bytes: &[u8]) -> Result<Map<String, Value>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "file is not UTF-8".to_string())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("expected a JSON object at the top level".into()),
        Err(error) => Err(format!("invalid JSON: {error}")),
    }
}

/// Write `bytes` to `path` atomically with owner-only permissions (0600 on
/// Unix), creating the parent directory when needed. Never writes through a
/// symlink because the publish step is a rename over the target path.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    crate::config::atomic_write(path, bytes).map_err(|error| error.to_string())
}

/// Pretty JSON with a trailing newline, the layout rpi writes for its own files.
pub(crate) fn pretty_json(value: &Value) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_ignores_key_order() {
        let a = json!({"b": 1, "a": {"y": true, "x": [1, {"d": 2, "c": 3}]}});
        let b = json!({"a": {"x": [1, {"c": 3, "d": 2}], "y": true}, "b": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(
            String::from_utf8(canonical_json(&a)).unwrap(),
            r#"{"a":{"x":[1,{"c":3,"d":2}],"y":true},"b":1}"#
        );
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn parse_object_strips_bom_and_rejects_non_objects() {
        let mut bytes = "\u{feff}".as_bytes().to_vec();
        bytes.extend_from_slice(br#"{"a":1}"#);
        assert_eq!(
            parse_object(&bytes).unwrap(),
            json!({"a": 1}).as_object().unwrap().clone()
        );
        assert!(parse_object(b"[1]").is_err());
        assert!(parse_object(b"{").is_err());
    }
}
