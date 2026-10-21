//! Private execution state. Public model snapshots never carry request headers
//! or credentials; requests recover their configuration by model identity here.
use super::*;

#[derive(Clone, Default)]
pub(super) enum RequestAuth {
    #[default]
    None,
    ApiKey(String),
    Headers(BTreeMap<String, String>),
}

pub(super) struct ExecutionBinding {
    provider_id: String,
    models: Vec<Model>,
    canonical_models: Vec<Model>,
    descriptors: Vec<Model>,
    auth: RequestAuth,
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
    model
}

impl ExecutionBinding {
    pub fn new(
        provider_id: &str,
        models: Vec<Model>,
        canonical_models: Vec<Model>,
        auth: RequestAuth,
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
            auth,
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
        let mut binding = Self::new(&self.provider_id, configured, models, self.auth.clone());
        binding.endpoint_override = self.endpoint_override.clone();
        binding.headers = self.headers.clone();
        binding.common_headers = self.common_headers.clone();
        binding.credential_headers = self.credential_headers.clone();
        binding
    }

    pub fn prepare(
        &self,
        requested: &Model,
        options: &SimpleStreamOptions,
    ) -> Option<(Model, SimpleStreamOptions)> {
        // The caller may select a published model, but cannot substitute an
        // endpoint or headers to redirect an already-bound credential.
        let mut model = self
            .models
            .iter()
            .find(|model| {
                requested.provider == self.provider_id
                    && model.provider == requested.provider
                    && model.api == requested.api
                    && model.id.eq_ignore_ascii_case(&requested.id)
            })?
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
                    && !matches!(&self.auth, RequestAuth::Headers(bound)
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
            match &self.auth {
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
