//! `settings.json`: copy only the keys rpi understands.
//!
//! The understood set mirrors the fields of [`crate::settings::Settings`]
//! (camelCase, including serde aliases). Each candidate value is also checked by
//! deserializing it into `Settings` on its own, so a value of the wrong type is
//! reported as a skip instead of breaking rpi's settings file.
//!
//! Three keys that rpi *could* read are deliberately not imported:
//!
//! - `sessionDir` would point rpi's own session storage at Pi's directory, so rpi
//!   would write into the source tree and mix formats.
//! - `extensions` and `extensionDirs` name Pi extension paths, which are not rpi
//!   cdylib directories. Loading native code from them would be an activation
//!   that nobody asked rpi to make.
//! - `lastRpiChangelogVersion` is rpi's own release-note baseline.

use serde_json::{Map, Value};
use std::path::Path;

use super::ledger::Ledger;
use super::options::Category;
use super::plan::{record, Item, Outcome, Plan};
use super::source;
use super::util::{canonical_json, canonical_sha256, pretty_json, write_private};
use crate::settings::Settings;

pub(crate) const DEST_FILE: &str = "settings.json";

/// Top-level keys rpi reads, including serde aliases.
const UNDERSTOOD: &[&str] = &[
    "defaultProvider",
    "defaultModel",
    "defaultThinkingLevel",
    "theme",
    "scopedModels",
    "enabledModels",
    "sessionDir",
    "shellPath",
    "shellCommandPrefix",
    "httpProxy",
    "retry",
    "compaction",
    "branchSummary",
    "steeringMode",
    "followUpMode",
    "skillDirs",
    "skills",
    "promptDirs",
    "prompts",
    "extensionDirs",
    "extensions",
    "keybindings",
    "doubleEscapeAction",
    "defaultTools",
    "hideThinkingBlock",
    "quietStartup",
    "lastChangelogVersion",
    "lastRpiChangelogVersion",
    "collapseChangelog",
    "showTerminalProgress",
    "editorPaddingX",
    "autocompleteMaxVisible",
    "fullscreenCopyOnSelect",
    "outputPad",
    "showCacheMissNotices",
    "showImages",
    "terminal",
    "httpIdleTimeout",
    "httpIdleTimeoutMs",
];

/// Keys that are understood but never imported, with the reason.
const NEVER_IMPORTED: &[(&str, &str)] = &[
    (
        "sessionDir",
        "would point rpi's session storage at Pi's directory",
    ),
    (
        "extensions",
        "Pi extension paths are not rpi cdylib directories",
    ),
    (
        "extensionDirs",
        "Pi extension paths are not rpi cdylib directories",
    ),
    (
        "lastRpiChangelogVersion",
        "rpi's own release-note baseline is never taken from Pi",
    ),
];

/// `(alias, canonical)` pairs. Imported values are written under the canonical
/// name; destination lookups accept either spelling.
const ALIASES: &[(&str, &str)] = &[
    ("enabledModels", "scopedModels"),
    ("skills", "skillDirs"),
    ("prompts", "promptDirs"),
    ("httpIdleTimeoutMs", "httpIdleTimeout"),
];

/// Nested objects rpi reads, with the sub-keys it reads from each.
const NESTED: &[(&str, &[&str])] = &[
    (
        "retry",
        &[
            "enabled",
            "maxRetries",
            "baseDelayMs",
            "maxAgentDelayMs",
            "provider",
        ],
    ),
    (
        "compaction",
        &[
            "enabled",
            "reserveTokens",
            "keepRecentTokens",
            "modelOverrides",
        ],
    ),
    ("branchSummary", &["reserveTokens", "skipPrompt"]),
    (
        "terminal",
        &["showImages", "showTerminalProgress", "clearOnShrink"],
    ),
];

/// `(key, value)` pairs to write, under canonical names.
pub(crate) type Writes = Vec<(String, Value)>;

pub(crate) fn plan(
    root: &Path,
    dest_settings: &Path,
    ledger: &Ledger,
) -> Result<Plan<Writes>, String> {
    let mut plan = Plan::new(Vec::new());
    let Some(bytes) = source::read_file(root, source::SETTINGS_FILE)? else {
        return Ok(plan);
    };
    let source_map = parse_tolerant(&bytes).map_err(|error| format!("settings.json: {error}"))?;
    let source_path = root
        .join(source::SETTINGS_FILE)
        .to_string_lossy()
        .into_owned();
    let destination = read_destination(dest_settings)?;
    let mut seen_canonical: Vec<&str> = Vec::new();

    for (key, value) in &source_map {
        if let Some((_, reason)) = NEVER_IMPORTED
            .iter()
            .find(|(name, _)| *name == key.as_str())
        {
            plan.items.push(ignore(key, reason));
            continue;
        }
        if !UNDERSTOOD.contains(&key.as_str()) {
            plan.items
                .push(ignore(key, "not a setting rpi understands"));
            continue;
        }
        let canonical = canonical_name(key);
        if seen_canonical.contains(&canonical) {
            plan.items
                .push(ignore(key, &format!("duplicate of {canonical}")));
            continue;
        }
        seen_canonical.push(canonical);

        if value.is_null() {
            plan.items.push(Item::new(
                Category::Settings,
                key,
                Outcome::Skip,
                "null value",
            ));
            continue;
        }
        let filtered = match nested_allowed(canonical) {
            None => value.clone(),
            Some(allowed) => match value.as_object() {
                None => {
                    plan.items.push(Item::new(
                        Category::Settings,
                        key,
                        Outcome::Skip,
                        "expected an object",
                    ));
                    continue;
                }
                Some(object) => {
                    let mut kept = Map::new();
                    for (sub, sub_value) in object {
                        if allowed.contains(&sub.as_str()) {
                            kept.insert(sub.clone(), sub_value.clone());
                        } else {
                            plan.items.push(ignore(
                                &format!("{key}.{sub}"),
                                "not a setting rpi understands",
                            ));
                        }
                    }
                    Value::Object(kept)
                }
            },
        };
        let mut single = Map::new();
        single.insert(canonical.to_string(), filtered.clone());
        if let Err(error) = serde_json::from_value::<Settings>(Value::Object(single)) {
            plan.items.push(Item::new(
                Category::Settings,
                key,
                Outcome::Skip,
                format!("rejected by rpi: {error}"),
            ));
            continue;
        }

        let sha = canonical_sha256(&filtered);
        let previous = ledger.lookup(Category::Settings.name(), &source_path, canonical);
        if previous.is_some_and(|record| record.sha256 == sha) {
            plan.items.push(Item::new(
                Category::Settings,
                key,
                Outcome::AlreadyImported,
                "same value as the last import",
            ));
            continue;
        }
        let changed = if previous.is_some() {
            "source changed since the last import; "
        } else {
            ""
        };
        let existing = destination.get(canonical).or_else(|| {
            ALIASES
                .iter()
                .find(|(_, target)| *target == canonical)
                .and_then(|(alias, _)| destination.get(*alias))
        });
        match existing {
            None => {
                plan.items.push(Item::new(
                    Category::Settings,
                    key,
                    Outcome::Import,
                    format!("{changed}{canonical}"),
                ));
                plan.records.push(record(
                    Category::Settings,
                    &source_path,
                    canonical,
                    &sha,
                    DEST_FILE,
                ));
                plan.writes.push((canonical.to_string(), filtered));
            }
            Some(existing) if canonical_json(existing) == canonical_json(&filtered) => {
                plan.items.push(Item::new(
                    Category::Settings,
                    key,
                    Outcome::AlreadyPresent,
                    format!("{changed}destination already has this value"),
                ));
                plan.records.push(record(
                    Category::Settings,
                    &source_path,
                    canonical,
                    &sha,
                    DEST_FILE,
                ));
            }
            Some(_) => {
                plan.items.push(Item::new(
                    Category::Settings,
                    key,
                    Outcome::Conflict,
                    format!("{changed}destination sets a different value; not overwritten"),
                ));
            }
        }
    }
    Ok(plan)
}

/// Merge the planned keys into the destination `settings.json`, re-checking each
/// key first. Unknown keys already in the destination are preserved.
pub(crate) fn apply(writes: &[(String, Value)], dest_settings: &Path) -> Result<(), String> {
    if writes.is_empty() {
        return Ok(());
    }
    let mut merged = read_destination(dest_settings)?;
    for (canonical, value) in writes {
        let existing = merged.get(canonical).cloned().or_else(|| {
            ALIASES
                .iter()
                .find(|(_, target)| *target == canonical.as_str())
                .and_then(|(alias, _)| merged.get(*alias).cloned())
        });
        match existing {
            None => {
                merged.insert(canonical.clone(), value.clone());
            }
            Some(existing) if canonical_json(&existing) == canonical_json(value) => {}
            Some(_) => {
                return Err(format!(
                    "{canonical} changed in the destination after planning; not overwritten"
                ))
            }
        }
    }
    let document = Value::Object(merged);
    serde_json::from_value::<Settings>(document.clone()).map_err(|error| {
        format!("merged settings would not load in rpi ({error}); nothing written")
    })?;
    write_private(dest_settings, &pretty_json(&document)?)
}

fn ignore(name: &str, reason: &str) -> Item {
    Item::new(Category::Settings, name, Outcome::Ignore, reason)
}

fn canonical_name(key: &str) -> &str {
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == key)
        .map_or(key, |(_, canonical)| canonical)
}

fn nested_allowed(key: &str) -> Option<&'static [&'static str]> {
    NESTED
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, allowed)| *allowed)
}

fn read_destination(path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            parse_tolerant(&bytes).map_err(|error| format!("destination settings.json: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(format!("could not read destination settings.json: {error}")),
    }
}

/// Parse a settings document, accepting `//` line comments the way rpi's own
/// loader does (pi allows them).
fn parse_tolerant(bytes: &[u8]) -> Result<Map<String, Value>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "file is not UTF-8".to_string())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let value = match serde_json::from_str::<Value>(text) {
        Ok(value) => value,
        Err(first) => serde_json::from_str::<Value>(&crate::config::strip_line_comments(text))
            .map_err(|_| format!("invalid JSON: {first}"))?,
    };
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("expected a JSON object at the top level".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_to_canonical_names() {
        assert_eq!(canonical_name("skills"), "skillDirs");
        assert_eq!(canonical_name("defaultModel"), "defaultModel");
        assert!(UNDERSTOOD.contains(&"skills"));
    }

    #[test]
    fn comments_are_tolerated_only_as_line_comments() {
        let map = parse_tolerant(b"{\n  // note\n  \"theme\": \"dark\"\n}\n").unwrap();
        assert_eq!(map["theme"], "dark");
        assert!(parse_tolerant(b"[]").is_err());
    }
}
