//! Host presentation contract for provider-owned, resumable login flows.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthInteraction {
    Input {
        title: String,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        secret: bool,
        #[serde(default, rename = "allowEmpty")]
        allow_empty: bool,
        continuation: Value,
    },
    Select {
        title: String,
        choices: Vec<AuthChoice>,
        continuation: Value,
    },
    Wait {
        #[serde(default)]
        message: Option<String>,
        #[serde(rename = "pollAfterMs")]
        poll_after_ms: u64,
        continuation: Value,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthChoice {
    pub id: String,
    pub label: String,
}

impl AuthInteraction {
    pub fn parse(value: Value) -> Result<Self, super::AuthError> {
        let interaction: Self =
            serde_json::from_value(value).map_err(|_| super::AuthError::Resolve)?;
        match &interaction {
            Self::Select { choices, .. } => {
                let unique: std::collections::BTreeSet<_> =
                    choices.iter().map(|choice| &choice.id).collect();
                if choices.is_empty()
                    || unique.len() != choices.len()
                    || choices.iter().any(|choice| choice.id.is_empty())
                {
                    return Err(super::AuthError::Resolve);
                }
            }
            Self::Wait { poll_after_ms, .. } if *poll_after_ms == 0 || *poll_after_ms > 60_000 => {
                return Err(super::AuthError::Resolve)
            }
            _ => {}
        }
        Ok(interaction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_ambiguous_choices_and_unbounded_polling_without_exposing_continuation() {
        for input in [
            serde_json::json!({"kind":"select","title":"Account","choices":[],"continuation":"secret"}),
            serde_json::json!({"kind":"select","title":"Account","choices":[{"id":"a","label":"A"},{"id":"a","label":"B"}],"continuation":"secret"}),
            serde_json::json!({"kind":"wait","pollAfterMs":60001,"continuation":"secret"}),
        ] {
            assert_eq!(
                AuthInteraction::parse(input).err(),
                Some(super::super::AuthError::Resolve)
            );
        }
        assert!(AuthInteraction::parse(serde_json::json!({"kind":"input","title":"Callback URL or code","continuation":{"state":"private"}})).is_ok());
    }
}
