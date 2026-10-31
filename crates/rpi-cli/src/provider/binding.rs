//! Private execution state. Public model snapshots never carry request headers
//! or credentials; requests recover their configuration by model identity here.
use super::*;
use rpi_runtime::credentials::{oauth_needs_refresh, OAuthRefresher};
use std::sync::{PoisonError, RwLock};

#[derive(Clone, Default)]
pub(super) enum RequestAuth {
    #[default]
    None,
    ApiKey(String),
    Headers(BTreeMap<String, String>),
}

impl RequestAuth {
    /// Whether `token` is the credential this auth sends.
    pub fn carries(&self, token: &str) -> bool {
        match self {
            Self::None => false,
            Self::ApiKey(key) => key == token,
            Self::Headers(headers) => headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("authorization")
                    && value.strip_prefix("Bearer ") == Some(token)
            }),
        }
    }

    /// Swap in a renewed token, keeping how it is sent.
    fn renew(&mut self, token: &str) {
        match self {
            Self::None => {}
            Self::ApiKey(key) => *key = token.into(),
            Self::Headers(headers) => {
                for (name, value) in headers.iter_mut() {
                    if name.eq_ignore_ascii_case("authorization") {
                        *value = format!("Bearer {token}");
                    }
                }
            }
        }
    }
}

/// Renewal state for auth bound from the host's stored OAuth credential.
pub(super) struct OAuthLease {
    refresher: Arc<dyn OAuthRefresher>,
    /// Serializes renewals; concurrent requests wait and reuse the result.
    expires: tokio::sync::Mutex<i64>,
}

impl OAuthLease {
    pub fn new(refresher: Arc<dyn OAuthRefresher>, expires: i64) -> Self {
        Self {
            refresher,
            expires: tokio::sync::Mutex::new(expires),
        }
    }
}

pub(super) struct ExecutionBinding {
    provider_id: String,
    models: Vec<Model>,
    canonical_models: Vec<Model>,
    descriptors: Vec<Model>,
    /// Shared with catalog rebinds so a renewal reaches every view of it.
    auth: Arc<RwLock<RequestAuth>>,
    oauth: Option<Arc<OAuthLease>>,
    endpoint_override: Option<String>,
    headers:
        Arc<std::collections::HashMap<(rpi_ai::Api, String), Option<BTreeMap<String, String>>>>,
    common_headers: Option<BTreeMap<String, String>>,
    credential_headers: std::collections::BTreeSet<String>,
}

impl std::fmt::Debug for ExecutionBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionBinding")
            .field("provider", &self.provider_id)
            .field("models", &self.models.len())
            .field("auth", &"[redacted]")
            .finish()
    }
}

pub(super) fn descriptor(model: &Model) -> Model {
    let mut model = model.clone();
    model.headers = None;
    // Endpoint userinfo, queries and fragments can carry credentials too. Only
    // the private binding needs the complete URL; public routing uses identity.
    if let Ok(mut url) = reqwest::Url::parse(&model.base_url) {
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            model.base_url = url.into();
        }
    }
    model
}

impl ExecutionBinding {
    pub fn new(
        provider_id: &str,
        models: Vec<Model>,
        canonical_models: Vec<Model>,
        auth: RequestAuth,
        oauth: Option<OAuthLease>,
    ) -> Self {
        let endpoint_override = models.iter().find_map(|bound| {
            canonical_models
                .iter()
                .find(|canonical| {
                    canonical.provider == bound.provider
                        && canonical.api == bound.api
                        && canonical.id.eq_ignore_ascii_case(&bound.id)
                })
                .filter(|canonical| canonical.base_url != bound.base_url)
                .map(|_| bound.base_url.clone())
        });
        let common_headers = models.first().and_then(|first| {
            models
                .iter()
                .all(|model| model.headers == first.headers)
                .then(|| first.headers.clone())
                .flatten()
        });
        let headers = Arc::new(
            models
                .iter()
                .map(|model| {
                    (
                        (model.api.clone(), model.id.to_ascii_lowercase()),
                        model.headers.clone(),
                    )
                })
                .collect(),
        );
        Self {
            provider_id: provider_id.into(),
            descriptors: models.iter().map(descriptor).collect(),
            models,
            canonical_models,
            auth: Arc::new(RwLock::new(auth)),
            oauth: oauth.map(Arc::new),
            endpoint_override,
            headers,
            common_headers,
            credential_headers: Default::default(),
        }
    }

    pub fn with_auth_config(mut self, auth: Option<&super::auth::AuthBinding>) -> Self {
        if let Some(auth) = auth {
            self.credential_headers = auth.credential_headers.clone();
        }
        self
    }

    pub fn models(&self) -> &[Model] {
        &self.descriptors
    }

    pub fn with_catalog(&self, models: Vec<Model>) -> Self {
        let mut configured = models.clone();
        for model in &mut configured {
            if let Some(endpoint) = &self.endpoint_override {
                model.base_url = endpoint.clone();
            }
            model.headers = self
                .headers
                .get(&(model.api.clone(), model.id.to_ascii_lowercase()))
                .cloned()
                .unwrap_or_else(|| self.common_headers.clone());
        }
        let mut binding = Self::new(
            &self.provider_id,
            configured,
            models,
            RequestAuth::None,
            None,
        );
        binding.auth = self.auth.clone();
        binding.oauth = self.oauth.clone();
        binding.endpoint_override = self.endpoint_override.clone();
        binding.headers = self.headers.clone();
        binding.common_headers = self.common_headers.clone();
        binding.credential_headers = self.credential_headers.clone();
        binding
    }

    fn bound_model(&self, requested: &Model) -> Option<&Model> {
        self.models.iter().find(|model| {
            requested.provider == self.provider_id
                && model.provider == requested.provider
                && model.api == requested.api
                && model.id.eq_ignore_ascii_case(&requested.id)
        })
    }

    pub fn binds(&self, requested: &Model) -> bool {
        self.bound_model(requested).is_some()
    }

    /// Renew a stored OAuth credential that has expired or is about to, before
    /// it is sent. An explicit request key or replacement auth bypasses it.
    pub async fn renew_oauth(&self, options: &SimpleStreamOptions) -> Result<(), String> {
        let Some(lease) = self.oauth.as_ref().filter(|_| {
            options.api_key.is_none()
                && !matches!(
                    options.authentication,
                    rpi_ai::auth::AttemptAuth::Replace(_)
                )
        }) else {
            return Ok(());
        };
        if matches!(
            &*self.auth.read().unwrap_or_else(PoisonError::into_inner),
            RequestAuth::Headers(_)
        ) && options.headers.as_ref().is_some_and(|headers| {
            headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization"))
        }) {
            return Ok(());
        }
        let mut expires = lease.expires.lock().await;
        if !oauth_needs_refresh(*expires, now_unix_seconds()) {
            return Ok(());
        }
        let refresher = Arc::clone(&lease.refresher);
        let provider = self.provider_id.clone();
        let renewed = tokio::task::spawn_blocking(move || refresher.refresh(&provider))
            .await
            .map_err(|_| "the refresh stopped unexpectedly".to_owned())??;
        if renewed.access.trim().is_empty()
            || (renewed.expires != 0 && renewed.expires <= now_unix_seconds())
        {
            return Err("the provider returned an invalid or expired credential".into());
        }
        self.auth
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .renew(&renewed.access);
        *expires = renewed.expires;
        Ok(())
    }

    pub fn prepare(
        &self,
        requested: &Model,
        options: &SimpleStreamOptions,
    ) -> Option<(Model, SimpleStreamOptions)> {
        // The caller may select a published model, but cannot substitute an
        // endpoint or headers to redirect an already-bound credential.
        let mut model = self.bound_model(requested)?.clone();
        let bound_auth = self
            .auth
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut options = options.clone();
        if let rpi_ai::auth::AttemptAuth::Replace(replacement) = &options.authentication {
            let auth = replacement.for_provider(&self.provider_id)?;
            let canonical = self.canonical_models.iter().find(|candidate| {
                candidate.provider == model.provider
                    && candidate.api == model.api
                    && candidate.id.eq_ignore_ascii_case(&model.id)
            })?;
            model.base_url = auth
                .base_url
                .clone()
                .unwrap_or_else(|| canonical.base_url.clone());
            // Bound headers may contain provider-specific credential names.
            // Strip every bound name as well as standard credential headers.
            let mut headers = options.headers.take().unwrap_or_default();
            headers.retain(|name, _| {
                ![
                    "authorization",
                    "proxy-authorization",
                    "x-api-key",
                    "api-key",
                    "cookie",
                ]
                .iter()
                .any(|credential| name.eq_ignore_ascii_case(credential))
                    && !self.credential_headers.contains(&name.to_ascii_lowercase())
                    && !model.headers.as_ref().is_some_and(|bound| {
                        bound.keys().any(|known| known.eq_ignore_ascii_case(name))
                    })
                    && !matches!(&bound_auth, RequestAuth::Headers(bound)
                        if bound.keys().any(|known| known.eq_ignore_ascii_case(name)))
            });
            for (name, value) in &auth.headers {
                headers.retain(|known, _| !known.eq_ignore_ascii_case(name));
                headers.insert(name.clone(), value.clone());
            }
            model.headers = None;
            options.api_key = auth.api_key.clone();
            options.headers = (!headers.is_empty()).then_some(headers);
            options.env = auth.env.clone();
            return Some((model, options));
        }
        if options.api_key.is_none() {
            match &bound_auth {
                RequestAuth::None => {}
                RequestAuth::ApiKey(key) => options.api_key = Some(key.clone()),
                RequestAuth::Headers(headers) => {
                    let mut merged = headers.clone();
                    for (name, value) in options.headers.take().unwrap_or_default() {
                        merged.retain(|known, _| !known.eq_ignore_ascii_case(&name));
                        merged.insert(name, value);
                    }
                    options.headers = Some(merged);
                }
            }
        }
        Some((model, options))
    }

    #[cfg(test)]
    pub fn request_model(&self, requested: &Model) -> Model {
        let (mut model, options) = self
            .prepare(requested, &SimpleStreamOptions::default())
            .unwrap();
        if let Some(headers) = &options.headers {
            merge_auth_headers(&mut model, headers);
        }
        if model.api != rpi_ai::Api::AnthropicMessages {
            if let Some(key) = options.api_key {
                merge_auth_headers(
                    &mut model,
                    &BTreeMap::from([("authorization".into(), format!("Bearer {key}"))]),
                );
            }
        }
        model
    }
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(i64::MAX, |duration| duration.as_secs() as i64)
}
