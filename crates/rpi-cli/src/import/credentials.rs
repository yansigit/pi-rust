//! `auth.json` credentials.
//!
//! Units of comparison and import are providers. A Pi entry is importable only
//! when it is complete:
//!
//! - `{"type":"oauth","access":<non-empty>,"refresh":<non-empty>,"expires":<integer ms>}`
//!   (any further fields are kept, as rpi's OAuth record keeps extra fields);
//! - `{"type":"api_key","key":<non-empty>}`, optionally with an `env` object of strings.
//!
//! **Expiry units.** Pi writes `expires` in Unix milliseconds. rpi's in-memory
//! credential is also milliseconds, but the on-disk `auth.json` read and written
//! by `CompatibilityCredentialStore` stores seconds (it multiplies by 1000 on
//! read and divides by 1000, rounding down, on write). Writing Pi's value into
//! that file verbatim would make every token look valid for about 30,000 years.
//! The import therefore never edits `auth.json` by hand: it hands the store a
//! millisecond credential, rounded down to whole seconds first so the stored and
//! compared values agree, and the store performs the conversion under its lock.

use std::collections::BTreeMap;
use std::path::Path;

use rpi_ai::auth::{Credential, CredentialStore, OAuthCredential};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::ledger::Ledger;
use super::options::Category;
use super::plan::{record, Item, Outcome, Plan};
use super::source;
use super::util::{canonical_sha256, parse_object};
use crate::config::auth_store::CompatibilityCredentialStore;

/// One credential to write through the store.
#[derive(Clone)]
pub(crate) struct CredentialWrite {
    pub(crate) provider: String,
    pub(crate) credential: Credential,
    pub(crate) oauth: bool,
}

pub(crate) type Writes = Vec<CredentialWrite>;

/// Round a millisecond expiry down to the whole second rpi stores, expressed
/// back in milliseconds. `None` when the store could not represent it.
pub(crate) fn stored_expiry_ms(expires_ms: i64) -> Option<i64> {
    expires_ms.div_euclid(1000).checked_mul(1000)
}

/// Plan the credential import from `root/auth.json` into `dest_auth`.
pub(crate) fn plan(root: &Path, dest_auth: &Path, ledger: &Ledger) -> Result<Plan<Writes>, String> {
    let mut plan = Plan::new(Vec::new());
    let Some(bytes) = source::read_file(root, source::AUTH_FILE)? else {
        return Ok(plan);
    };
    let source_map = parse_object(&bytes).map_err(|error| format!("auth.json: {error}"))?;
    let source_path = root.join(source::AUTH_FILE).to_string_lossy().into_owned();
    let destination = read_destination(dest_auth)?;

    for (provider, value) in &source_map {
        let sha = canonical_sha256(value);
        let previous = ledger.lookup(Category::Credentials.name(), &source_path, provider);
        if previous.is_some_and(|record| record.sha256 == sha) {
            plan.items.push(Item::new(
                Category::Credentials,
                provider,
                Outcome::AlreadyImported,
                "same content as the last import",
            ));
            continue;
        }
        let changed = if previous.is_some() {
            "source changed since the last import; "
        } else {
            ""
        };

        let credential = match source_credential(value) {
            Ok(credential) => credential,
            Err(reason) => {
                plan.items.push(Item::new(
                    Category::Credentials,
                    provider,
                    Outcome::Skip,
                    format!("{changed}{reason}"),
                ));
                continue;
            }
        };
        let oauth = matches!(credential, Credential::Oauth(_));
        let describe = describe(&credential);

        match destination.get(provider) {
            None => {
                plan.items.push(Item::new(
                    Category::Credentials,
                    provider,
                    Outcome::Import,
                    format!("{changed}{describe}"),
                ));
                plan.records.push(record(
                    Category::Credentials,
                    &source_path,
                    provider,
                    &sha,
                    "auth.json",
                ));
                plan.writes.push(CredentialWrite {
                    provider: provider.clone(),
                    credential,
                    oauth,
                });
            }
            Some(existing) => {
                let existing = stored_credential(existing)
                    .map_err(|error| format!("destination auth.json: {provider}: {error}"))?;
                if existing == credential {
                    plan.items.push(Item::new(
                        Category::Credentials,
                        provider,
                        Outcome::AlreadyPresent,
                        format!("{changed}destination already holds this credential"),
                    ));
                    plan.records.push(record(
                        Category::Credentials,
                        &source_path,
                        provider,
                        &sha,
                        "auth.json",
                    ));
                } else {
                    plan.items.push(Item::new(
                        Category::Credentials,
                        provider,
                        Outcome::Conflict,
                        format!(
                            "{changed}destination holds a different credential; not overwritten"
                        ),
                    ));
                }
            }
        }
    }
    Ok(plan)
}

/// Write the planned credentials through the compatibility store, one provider
/// at a time under its lock. Each write re-checks the destination so a credential
/// that appeared after planning is refused rather than overwritten.
pub(crate) async fn apply(writes: &[CredentialWrite], dest_auth: &Path) -> Result<(), String> {
    if writes.is_empty() {
        return Ok(());
    }
    let store = CompatibilityCredentialStore::new(dest_auth.to_path_buf())
        .map_err(|error| format!("could not use {}: {error}", dest_auth.display()))?;
    for write in writes {
        let mut transaction = store
            .lock(&write.provider, CancellationToken::new())
            .await
            .map_err(|error| format!("could not lock auth.json: {error}"))?;
        match transaction.current() {
            None => transaction
                .replace(write.credential.clone())
                .await
                .map_err(|error| {
                    format!("could not write the {} credential: {error}", write.provider)
                })?,
            Some(current) if current == write.credential => {}
            Some(_) => {
                return Err(format!(
                    "{} changed in the destination after planning; not overwritten",
                    write.provider
                ))
            }
        }
    }
    Ok(())
}

fn read_destination(path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            parse_object(&bytes).map_err(|error| format!("destination auth.json: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(format!("could not read destination auth.json: {error}")),
    }
}

fn describe(credential: &Credential) -> String {
    match credential {
        Credential::Oauth(oauth) => format!(
            "oauth, expiry {} ms (stored as {} s)",
            oauth.expires_ms,
            oauth.expires_ms.div_euclid(1000)
        ),
        Credential::ApiKey { .. } => "api_key".into(),
    }
}

/// Convert one Pi `auth.json` entry into an rpi credential, or say why it is not
/// importable.
fn source_credential(value: &Value) -> Result<Credential, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "entry is not a JSON object".to_string())?;
    match object.get("type").and_then(Value::as_str) {
        Some("oauth") => {
            let access = non_empty_string(object, "access")
                .ok_or_else(|| "incomplete oauth entry: access is missing".to_string())?;
            let refresh = non_empty_string(object, "refresh")
                .ok_or_else(|| "incomplete oauth entry: refresh is missing".to_string())?;
            let expires_ms = object
                .get("expires")
                .and_then(Value::as_i64)
                .ok_or_else(|| {
                    "incomplete oauth entry: expires is not an integer millisecond timestamp"
                        .to_string()
                })?;
            let expires_ms = stored_expiry_ms(expires_ms)
                .ok_or_else(|| "oauth expiry is out of range".to_string())?;
            let extra: BTreeMap<String, Value> = object
                .iter()
                .filter(|(key, _)| {
                    !matches!(key.as_str(), "type" | "access" | "refresh" | "expires")
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            Ok(Credential::Oauth(OAuthCredential {
                access,
                refresh,
                expires_ms,
                extra,
            }))
        }
        Some("api_key") => {
            let key = non_empty_string(object, "key")
                .ok_or_else(|| "incomplete api_key entry: key is missing".to_string())?;
            let env = match object.get("env") {
                None => BTreeMap::new(),
                Some(Value::Object(map)) => {
                    let mut env = BTreeMap::new();
                    for (name, value) in map {
                        let value = value
                            .as_str()
                            .ok_or_else(|| "api_key env values must be strings".to_string())?;
                        env.insert(name.clone(), value.to_string());
                    }
                    env
                }
                Some(_) => return Err("api_key env must be an object".into()),
            };
            Ok(Credential::ApiKey {
                key: Some(key),
                env,
            })
        }
        Some(other) => Err(format!("unsupported credential type `{other}`")),
        None => Err("credential has no type".into()),
    }
}

/// Read a destination entry the way `CompatibilityCredentialStore` reads it:
/// OAuth expiry is seconds on disk and becomes milliseconds in memory, and a
/// `null` env is dropped. Nothing is executed (no `!command` resolution), so
/// planning stays free of side effects.
fn stored_credential(value: &Value) -> Result<Credential, String> {
    let mut value = value.clone();
    if value.get("type").and_then(Value::as_str) == Some("oauth") {
        let seconds = match value.get("expires") {
            None => 0,
            Some(expires) => expires
                .as_i64()
                .ok_or_else(|| "expires is not an integer".to_string())?,
        };
        let milliseconds = seconds
            .checked_mul(1000)
            .ok_or_else(|| "expires is out of range".to_string())?;
        value["expires"] = milliseconds.into();
    }
    if value.get("env") == Some(&Value::Null) {
        if let Some(object) = value.as_object_mut() {
            object.remove("env");
        }
    }
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn non_empty_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn expiry_rounds_down_to_whole_seconds() {
        assert_eq!(stored_expiry_ms(1_800_000_000_999), Some(1_800_000_000_000));
        assert_eq!(stored_expiry_ms(-1), Some(-1000));
        assert_eq!(stored_expiry_ms(i64::MIN), None);
    }

    #[test]
    fn complete_entries_convert_and_incomplete_ones_explain_why() {
        let oauth = json!({"type":"oauth","access":"a","refresh":"r","expires":1_800_000_000_999_i64,"projectId":"p"});
        match source_credential(&oauth).unwrap() {
            Credential::Oauth(credential) => {
                assert_eq!(credential.expires_ms, 1_800_000_000_000);
                assert_eq!(credential.extra["projectId"], "p");
                assert!(!credential.extra.contains_key("expires"));
            }
            _ => panic!("expected oauth"),
        }
        let cases = [
            (
                json!({"type":"oauth","access":"a","expires":1}),
                "refresh is missing",
            ),
            (
                json!({"type":"oauth","access":"a","refresh":"r","expires":1.5}),
                "expires is not",
            ),
            (
                json!({"type":"oauth","access":"a","refresh":"r"}),
                "expires is not",
            ),
            (json!({"type":"api_key"}), "key is missing"),
            (
                json!({"type":"api_key","key":"k","env":{"A":1}}),
                "must be strings",
            ),
            (json!({"type":"mystery"}), "unsupported credential type"),
            (json!("text"), "not a JSON object"),
        ];
        for (value, expected) in cases {
            let error = source_credential(&value).err().unwrap();
            assert!(
                error.contains(expected),
                "{error} should mention {expected}"
            );
        }
    }

    #[test]
    fn destination_seconds_read_back_as_milliseconds() {
        let stored = json!({"type":"oauth","access":"a","refresh":"r","expires":1_800_000_000_i64});
        match stored_credential(&stored).unwrap() {
            Credential::Oauth(credential) => assert_eq!(credential.expires_ms, 1_800_000_000_000),
            _ => panic!("expected oauth"),
        }
    }
}
