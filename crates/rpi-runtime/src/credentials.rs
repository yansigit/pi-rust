//! Shared, provider-scoped credential resolution for requests and auth UI.
//! Provider packages own their environment names and anonymous policy. The host
//! owns persistence and precedence, and never guesses a key from a wire protocol.
use crate::config::{AuthStore, Credential, ProviderConfig};
use crate::ProviderDeclaration as ProviderManifest;
use crate::ValueSource;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum CredentialSource {
    /// A host-supplied override scoped to the selected provider.
    Explicit,
    StoredApiKey,
    StoredOauth,
    ModelsJson,
    Environment(String),
    Headers,
    Anonymous,
    Missing,
}

/// Local readiness only; this does not claim that a server accepted the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthStatus {
    Missing,
    Ready,
    Expired,
    Anonymous,
}

/// Safe for UI snapshots and change notifications; contains no credential values.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AuthState {
    pub status: AuthStatus,
    pub source: CredentialSource,
}

impl AuthState {
    pub fn is_ready(&self) -> bool {
        matches!(self.status, AuthStatus::Ready | AuthStatus::Anonymous)
    }
}

pub(crate) fn state(
    provider: &str,
    config: Option<&ProviderConfig>,
    store: &AuthStore,
    manifest: Option<&ProviderManifest>,
    values: &dyn ValueSource,
    has_headers: impl FnOnce() -> bool,
) -> AuthState {
    let credential = resolve(provider, config, store, manifest, values);
    let source = if credential.source.is_present() {
        credential.source
    } else if has_headers() {
        CredentialSource::Headers
    } else {
        CredentialSource::Missing
    };
    let status = match source {
        CredentialSource::Anonymous => AuthStatus::Anonymous,
        CredentialSource::Missing => match store.get(provider) {
            Some(Credential::Oauth { expires, .. })
                if *expires != 0 && *expires <= values.now_unix_seconds() =>
            {
                AuthStatus::Expired
            }
            _ => AuthStatus::Missing,
        },
        _ => AuthStatus::Ready,
    };
    AuthState { status, source }
}

impl CredentialSource {
    pub fn is_present(&self) -> bool {
        !matches!(self, Self::Missing)
    }

    pub fn as_json_str(&self) -> &str {
        match self {
            Self::Explicit => "override",
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
pub struct ResolvedCredential {
    pub key: Option<String>,
    pub source: CredentialSource,
    pub bearer: bool,
}

/// Only these identities have a built-in environment fallback. Other providers
/// need a loaded declaration or an explicit models.json apiKey reference.
pub fn builtin_api_key_env(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        _ => None,
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|key| !key.trim().is_empty())
}

pub fn resolve(
    provider: &str,
    config: Option<&ProviderConfig>,
    store: &AuthStore,
    manifest: Option<&ProviderManifest>,
    values: &dyn ValueSource,
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
            if let Some(key) = nonempty(crate::values::resolve_config_value(
                key,
                env.as_ref(),
                values,
                true,
            )) {
                return credential(key, CredentialSource::StoredApiKey, bearer);
            }
        }
        Some(Credential::Oauth {
            access, expires, ..
        }) if !access.trim().is_empty() => {
            let now = values.now_unix_seconds();
            if *expires == 0 || *expires > now {
                return credential(access.clone(), CredentialSource::StoredOauth, true);
            }
        }
        _ => {}
    }
    if let Some(key) = config
        .and_then(|config| config.api_key.as_deref())
        .and_then(|raw| nonempty(crate::values::resolve_config_value(raw, None, values, true)))
    {
        return credential(key, CredentialSource::ModelsJson, bearer);
    }
    // A package declaration is authoritative even for a built-in identity.
    if manifest.is_none() && provider == "anthropic" {
        if let Some(key) = nonempty(values.env("ANTHROPIC_AUTH_TOKEN")) {
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
        if let Some(key) = nonempty(values.env(name)) {
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

pub fn has_header_auth(headers: Option<&std::collections::BTreeMap<String, String>>) -> bool {
    headers.is_some_and(|headers| {
        headers.iter().any(|(name, value)| {
            ["authorization", "x-api-key", "cf-aig-authorization"]
                .contains(&name.to_ascii_lowercase().as_str())
                && !value.trim().is_empty()
        })
    })
}
