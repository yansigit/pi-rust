//! Shared, provider-scoped credential resolution for requests and auth UI.
//! Provider packages own their environment names and anonymous policy. The host
//! owns persistence and precedence, and never guesses a key from a wire protocol.
use crate::config::{self, AuthStore, Credential, ProviderConfig};
use rpi_extensions::ProviderManifest;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CredentialSource {
    StoredApiKey,
    StoredOauth,
    ModelsJson,
    Environment(String),
    Headers,
    Anonymous,
    Missing,
}

impl CredentialSource {
    pub fn is_present(&self) -> bool {
        !matches!(self, Self::Missing)
    }

    pub fn as_json_str(&self) -> &str {
        match self {
            Self::StoredApiKey => "auth.json",
            Self::StoredOauth => "oauth",
            Self::ModelsJson => "models.json",
            Self::Environment(name) => name,
            Self::Headers => "headers",
            Self::Anonymous => "anonymous",
            Self::Missing => "none",
        }
    }
}

// Deliberately no Debug: this object contains secret material.
pub(crate) struct ResolvedCredential {
    pub key: Option<String>,
    pub source: CredentialSource,
    pub bearer: bool,
}

/// Only these identities have a built-in environment fallback. Other providers
/// need a loaded declaration or an explicit models.json apiKey reference.
pub(crate) fn builtin_api_key_env(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        _ => None,
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|key| !key.trim().is_empty())
}

pub(crate) fn resolve(
    provider: &str,
    config: Option<&ProviderConfig>,
    store: &AuthStore,
    manifest: Option<&ProviderManifest>,
) -> ResolvedCredential {
    let bearer = config
        .and_then(|config| config.auth_header)
        .unwrap_or(false);
    let credential = |key, source, bearer| ResolvedCredential {
        key: Some(key),
        source,
        bearer,
    };
    match store.get(provider) {
        Some(Credential::ApiKey {
            key: Some(key),
            env,
        }) => {
            if let Some(key) = nonempty(config::resolve_config_value(key, env.as_ref())) {
                return credential(key, CredentialSource::StoredApiKey, bearer);
            }
        }
        Some(Credential::Oauth {
            access, expires, ..
        }) if !access.trim().is_empty() => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(i64::MAX);
            if *expires == 0 || *expires > now {
                return credential(access.clone(), CredentialSource::StoredOauth, true);
            }
        }
        _ => {}
    }
    if let Some(key) = config
        .and_then(|config| config.api_key.as_deref())
        .and_then(|raw| nonempty(config::resolve_config_value(raw, None)))
    {
        return credential(key, CredentialSource::ModelsJson, bearer);
    }
    // A package declaration is authoritative even for a built-in identity.
    if manifest.is_none() && provider == "anthropic" {
        if let Some(key) = nonempty(std::env::var("ANTHROPIC_AUTH_TOKEN").ok()) {
            return credential(
                key,
                CredentialSource::Environment("ANTHROPIC_AUTH_TOKEN".into()),
                true,
            );
        }
    }
    let env = match manifest {
        Some(manifest) => manifest.api_key_env.as_deref(),
        None => builtin_api_key_env(provider),
    };
    if let Some(name) = env {
        if let Some(key) = nonempty(std::env::var(name).ok()) {
            return credential(key, CredentialSource::Environment(name.into()), bearer);
        }
    }
    ResolvedCredential {
        key: None,
        source: if manifest.is_some_and(|manifest| !manifest.requires_auth) {
            CredentialSource::Anonymous
        } else {
            CredentialSource::Missing
        },
        bearer,
    }
}

pub(crate) fn has_header_auth(
    headers: Option<&std::collections::BTreeMap<String, String>>,
) -> bool {
    headers.is_some_and(|headers| {
        headers.iter().any(|(name, value)| {
            ["authorization", "x-api-key", "cf-aig-authorization"]
                .contains(&name.to_ascii_lowercase().as_str())
                && !value.trim().is_empty()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_environment_requires_an_explicit_package_declaration() {
        let _guard = config::test_support::env_lock().lock().unwrap();
        let previous = std::env::var_os("DEEPSEEK_API_KEY");
        std::env::set_var("DEEPSEEK_API_KEY", "fixture-package-key");
        let store = AuthStore::new();
        let without_package = resolve("deepseek", None, &store, None);
        let manifest = ProviderManifest {
            replace_catalog: false,
            id: "deepseek".into(),
            models: vec![],
            api_key_env: Some("DEEPSEEK_API_KEY".into()),
            requires_auth: true,
        };
        let with_package = resolve("deepseek", None, &store, Some(&manifest));
        match previous {
            Some(value) => std::env::set_var("DEEPSEEK_API_KEY", value),
            None => std::env::remove_var("DEEPSEEK_API_KEY"),
        }
        assert_eq!(without_package.source, CredentialSource::Missing);
        assert!(without_package.key.is_none());
        assert_eq!(
            with_package.source,
            CredentialSource::Environment("DEEPSEEK_API_KEY".into())
        );
        assert_eq!(with_package.key.as_deref(), Some("fixture-package-key"));
    }

    #[test]
    fn package_policy_can_disable_a_builtin_environment_fallback() {
        let _guard = config::test_support::env_lock().lock().unwrap();
        let previous = std::env::var_os("OPENAI_API_KEY");
        std::env::set_var("OPENAI_API_KEY", "unrelated-builtin-key");
        let manifest = ProviderManifest {
            replace_catalog: false,
            id: "openai".into(),
            models: vec![],
            api_key_env: None,
            requires_auth: true,
        };
        let resolved = resolve("openai", None, &AuthStore::new(), Some(&manifest));
        match previous {
            Some(value) => std::env::set_var("OPENAI_API_KEY", value),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        assert_eq!(resolved.source, CredentialSource::Missing);
        assert!(resolved.key.is_none());
    }

    #[test]
    fn empty_keys_do_not_block_fallback_or_make_required_providers_ready() {
        let config: ProviderConfig =
            serde_json::from_value(serde_json::json!({"apiKey":"   ","models":[]})).unwrap();
        let mut store = AuthStore::new();
        store.insert(
            "gateway".into(),
            Credential::ApiKey {
                key: Some("".into()),
                env: None,
            },
        );
        let mut manifest = ProviderManifest {
            replace_catalog: false,
            id: "gateway".into(),
            models: vec![],
            api_key_env: None,
            requires_auth: true,
        };
        assert_eq!(
            resolve("gateway", Some(&config), &store, Some(&manifest)).source,
            CredentialSource::Missing
        );
        manifest.requires_auth = false;
        assert_eq!(
            resolve("gateway", Some(&config), &store, Some(&manifest)).source,
            CredentialSource::Anonymous
        );
    }
}
