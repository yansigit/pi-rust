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
    descriptors: Vec<Model>,
    auth: RequestAuth,
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
    pub fn new(provider_id: &str, models: Vec<Model>, auth: RequestAuth) -> Self {
        Self {
            provider_id: provider_id.into(),
            descriptors: models.iter().map(descriptor).collect(),
            models,
            auth,
        }
    }

    pub fn descriptors(&self) -> &[Model] {
        &self.descriptors
    }

    pub fn prepare(
        &self,
        requested: &Model,
        options: &SimpleStreamOptions,
    ) -> Option<(Model, SimpleStreamOptions)> {
        // The caller may select a published model, but cannot substitute an
        // endpoint or headers to redirect an already-bound credential.
        let model = self
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
