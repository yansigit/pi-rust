//! Declarative native dialogs shared by local and remote hosts.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

/// JavaScript trim/search semantics differ from Rust's Unicode whitespace set.
pub fn is_javascript_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DialogKind {
    Input,
    Select,
    Confirm,
    Settings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Choice {
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Setting {
    pub id: String,
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub submenu: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Dialog {
    pub kind: DialogKind,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub settings: Vec<Setting>,
    #[serde(default)]
    pub searchable: bool,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub selected: Option<String>,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub freeform: bool,
    #[serde(default)]
    pub initial_value: String,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub masked: bool,
    #[serde(default)]
    pub trim: bool,
    #[serde(default)]
    pub integer_min: Option<u64>,
}

impl Dialog {
    pub fn parse(value: &Value) -> Result<Self, String> {
        if value.to_string().len() > 4 * 1024 * 1024 {
            return Err("native dialog exceeds the size limit".into());
        }
        let dialog: Self = serde_json::from_value(value.clone())
            .map_err(|_| "invalid native dialog specification")?;
        if dialog.choices.len() > 8192 || dialog.settings.len() > 8192 {
            return Err("native dialog has too many rows".into());
        }
        let mut values = HashSet::new();
        if dialog
            .choices
            .iter()
            .any(|item| !values.insert(&item.value))
        {
            return Err("native dialog choice values must be unique".into());
        }
        values.clear();
        if dialog
            .settings
            .iter()
            .any(|item| item.id.is_empty() || !values.insert(&item.id))
        {
            return Err("native dialog setting IDs must be nonempty and unique".into());
        }
        if dialog
            .integer_min
            .is_some_and(|min| min > 9_007_199_254_740_991)
        {
            return Err("integer minimum exceeds the exact integer range".into());
        }
        if dialog.kind != DialogKind::Select && (dialog.multiple || dialog.freeform) {
            return Err("multiple/freeform choices require a select dialog".into());
        }
        if dialog.kind != DialogKind::Select && !dialog.choices.is_empty() {
            return Err("choices require a select dialog".into());
        }
        if dialog.kind != DialogKind::Settings && !dialog.settings.is_empty() {
            return Err("settings rows require a settings dialog".into());
        }
        if dialog.kind != DialogKind::Input
            && (dialog.integer_min.is_some()
                || dialog.masked
                || dialog.trim
                || !dialog.initial_value.is_empty())
        {
            return Err("input controls require an input dialog".into());
        }
        Ok(dialog)
    }

    /// Validate remote replies too; never include a supplied secret in an error.
    pub fn answer(&self, answer: &Value) -> Result<Value, String> {
        if answer.to_string().len() > 4 * 1024 * 1024 {
            return Err("native dialog answer exceeds the size limit".into());
        }
        let action = answer["type"]
            .as_str()
            .ok_or("native dialog answer requires a type")?;
        let query = || -> Result<&str, String> {
            match answer.get("query") {
                None => Ok(""),
                Some(value) => value.as_str().ok_or_else(|| "invalid dialog query".into()),
            }
        };
        match (self.kind, action) {
            (DialogKind::Input, "input") => {
                let raw = answer["value"]
                    .as_str()
                    .ok_or("input answer requires text")?;
                let value = if self.trim || self.integer_min.is_some() {
                    raw.trim_matches(is_javascript_whitespace)
                } else {
                    raw
                };
                if let Some(min) = self.integer_min {
                    let integer = (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                        .then(|| value.parse::<u64>().ok())
                        .flatten();
                    if !integer.is_some_and(|n| n >= min && n <= 9_007_199_254_740_991) {
                        return Err(format!("Enter an integer greater than or equal to {min}."));
                    }
                    return Ok(json!({"type":"input","value":integer.unwrap().to_string()}));
                }
                Ok(json!({"type":"input","value":value}))
            }
            (DialogKind::Confirm, "confirm") => {
                let confirmed = answer["confirmed"]
                    .as_bool()
                    .ok_or("confirmation requires a boolean")?;
                Ok(json!({"type":"confirm","confirmed":confirmed}))
            }
            (DialogKind::Select, "select" | "freeform") => {
                if action == "freeform" && !self.freeform {
                    return Err("freeform selection is unavailable".into());
                }
                let selected = answer["values"]
                    .as_array()
                    .ok_or("selection requires values")?;
                if !self.multiple && selected.len() != usize::from(action == "select") {
                    return Err("select exactly one choice".into());
                }
                let mut seen = HashSet::new();
                for value in selected {
                    let value = value.as_str().ok_or("invalid selected value")?;
                    if !seen.insert(value) || !self.choices.iter().any(|item| item.value == value) {
                        return Err("invalid or duplicate selected value".into());
                    }
                }
                Ok(json!({"type":action,"values":selected,"query":query()?}))
            }
            (DialogKind::Settings, "open" | "change") => {
                let id = answer["id"]
                    .as_str()
                    .ok_or("setting answer requires an ID")?;
                let item = self
                    .settings
                    .iter()
                    .find(|item| item.id == id)
                    .ok_or("unknown setting ID")?;
                if action == "open" && item.submenu {
                    return Ok(json!({"type":"open","id":id,"query":query()?}));
                }
                let value = answer["value"]
                    .as_str()
                    .ok_or("setting change requires a value")?;
                if action != "change"
                    || item.submenu
                    || !item.values.iter().any(|known| known == value)
                {
                    return Err("unsupported setting change".into());
                }
                Ok(json!({"type":"change","id":id,"value":value,"query":query()?}))
            }
            _ => Err("answer does not match the native dialog".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn choices_use_ids_even_when_labels_match_and_reject_forged_values() {
        let dialog = Dialog::parse(&json!({"kind":"select","title":"Account","choices":[{"value":"one","label":"Same"},{"value":"two","label":"Same"}]})).unwrap();
        assert_eq!(
            dialog
                .answer(&json!({"type":"select","values":["two"]}))
                .unwrap()["values"][0],
            "two"
        );
        assert!(dialog
            .answer(&json!({"type":"select","values":["Same"]}))
            .is_err());
        assert!(dialog
            .answer(&json!({"type":"select","values":["one","two"]}))
            .is_err());
        assert!(Dialog::parse(&json!({"kind":"select","title":"Account","choices":[{"value":"one","label":"A"},{"value":"one","label":"B"}]})).is_err());
    }
    #[test]
    fn integer_inputs_match_upstream_decimal_safe_integer_validation() {
        let dialog =
            Dialog::parse(&json!({"kind":"input","title":"Weight","integerMin":1})).unwrap();
        assert_eq!(
            dialog
                .answer(&json!({"type":"input","value":" 002 "}))
                .unwrap()["value"],
            "2"
        );
        assert_eq!(
            dialog
                .answer(&json!({"type":"input","value":"\u{feff}003\u{feff}"}))
                .unwrap()["value"],
            "3"
        );
        assert!(dialog
            .answer(&json!({"type":"input","value":"\u{0085}3"}))
            .is_err());
        for value in [
            "0",
            "-1",
            "1.0",
            "1e2",
            "",
            "9007199254740992",
            "secret-text",
        ] {
            assert_eq!(
                dialog
                    .answer(&json!({"type":"input","value":value}))
                    .unwrap_err(),
                "Enter an integer greater than or equal to 1."
            );
        }
    }
    #[test]
    fn nested_settings_only_accept_declared_edits_and_open_actions() {
        let dialog = Dialog::parse(&json!({"kind":"settings","title":"Pool","settings":[{"id":"enabled","label":"Enabled","value":"yes","values":["yes","no"]},{"id":"accounts","label":"Accounts","value":"2","submenu":true}]})).unwrap();
        assert!(dialog
            .answer(&json!({"type":"change","id":"enabled","value":"no"}))
            .is_ok());
        assert!(dialog
            .answer(&json!({"type":"open","id":"accounts"}))
            .is_ok());
        assert!(dialog
            .answer(&json!({"type":"open","id":"enabled"}))
            .is_err());
        assert!(dialog
            .answer(&json!({"type":"change","id":"accounts","value":"no"}))
            .is_err());
        assert!(dialog
            .answer(&json!({"type":"change","id":"enabled","value":"other"}))
            .is_err());
        assert!(Dialog::parse(&json!({"kind":"unknown","title":"Bad"})).is_err());
        assert!(Dialog::parse(&json!({"kind":"input","title":"Bad","unknown":true})).is_err());
    }
}
