//! Session-owned provider auth, resolved anew for each physical request.
use std::sync::Arc;

use rpi_ai::auth::{
    AuthContext, AuthError, AuthOptions, AuthRuntime, CredentialStore, ProviderAuth, SystemClock,
};
use rpi_ai::{Model, SimpleStreamOptions};

#[derive(Clone)]
pub(super) struct AuthBinding {
    pub provider: Arc<dyn ProviderAuth>,
    pub runtime: Arc<AuthRuntime>,
    pub store: Result<Arc<dyn CredentialStore>, AuthError>,
    pub context: AuthContext,
    pub operations: Option<Arc<rpi_extensions::provider_runtime::NativeProviderOperations>>,
    pub model_headers:
        std::collections::HashMap<(String, String), std::collections::BTreeMap<String, String>>,
    pub credential_headers: std::collections::BTreeSet<String>,
}

impl AuthBinding {
    pub async fn prepare(
        &self,
        model: &mut Model,
        options: &mut SimpleStreamOptions,
    ) -> Result<(), AuthError> {
        let mut provider = self.provider.clone();
        let override_auth = if let Some(operations) = &self.operations {
            let mut env = self.context.env.clone();
            env.extend(
                options
                    .env
                    .iter()
                    .filter(|(_, value)| !value.is_empty())
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
            let input = serde_json::json!({"modelId":model.id,"api":model.api,"apiKey":options.api_key,"env":env});
            let operations = operations.clone();
            let result = self
                .runtime
                .run_owned(options.signal.clone(), move |signal| async move {
                    operations
                        .request("requestAuth", input, signal.clone())
                        .await
                        .map_err(|error| {
                            if signal.is_cancelled() {
                                AuthError::Cancelled
                            } else {
                                AuthError::Provider(error)
                            }
                        })
                })
                .await?;
            if result.is_null() {
                None
            } else if let Some(policy) = result.get("resolve") {
                if result.as_object().map_or(true, |value| value.len() != 1) {
                    return Err(AuthError::Resolve);
                }
                let policy: super::auth_policy::Policy =
                    serde_json::from_value(policy.clone()).map_err(|_| AuthError::Resolve)?;
                if policy.fallback_api_key.is_some()
                    && (!policy.api_key || !provider.supports_api_key())
                {
                    return Err(AuthError::Resolve);
                }
                provider = Arc::new(super::auth_policy::Restricted {
                    inner: provider,
                    policy,
                });
                None
            } else {
                Some(
                    serde_json::from_value::<rpi_ai::auth::RequestAuth>(result)
                        .map_err(|_| AuthError::Resolve)?,
                )
            }
        } else {
            None
        };
        let resolved = override_auth.is_none();
        let mut auth = match override_auth {
            Some(auth) => auth,
            None => self
                .runtime
                .resolve(
                    model.provider.clone(),
                    provider,
                    self.store.clone().unwrap_or_else(|error| {
                        Arc::new(super::auth_policy::UnavailableStore(error))
                    }),
                    self.context.clone(),
                    AuthOptions {
                        api_key: options.api_key.clone(),
                        env: options.env.clone(),
                        minimum_validity_ms: None,
                        force_refresh: false,
                    },
                    options.signal.clone(),
                    Arc::new(SystemClock),
                )
                .await?
                .ok_or(AuthError::Resolve)?,
        };
        if resolved {
            if let Some(headers) = self
                .model_headers
                .get(&(model.api.as_str().to_owned(), model.id.clone()))
            {
                let mut explicit = auth.env.clone();
                explicit.extend(options.env.clone());
                let env = crate::config::native_values::overlay(&self.context.env, &explicit);
                for (name, raw) in headers {
                    let value = crate::config::native_values::resolve(
                        raw,
                        &format!(
                            "model {:?} header {name:?}",
                            format!("{}/{}", model.provider, model.id)
                        ),
                        &env,
                    )
                    .await
                    .map_err(AuthError::Provider)?;
                    auth.headers
                        .retain(|known, _| !known.eq_ignore_ascii_case(name));
                    auth.headers.insert(name.clone(), value);
                }
            }
        }
        if !resolved || options.api_key.is_none() {
            options.api_key = auth.api_key;
        }
        if let Some(base_url) = auth.base_url {
            model.base_url = base_url;
        }
        let (mut headers, overrides) = if resolved {
            (auth.headers, options.headers.take().unwrap_or_default())
        } else {
            (options.headers.take().unwrap_or_default(), auth.headers)
        };
        for (name, value) in overrides {
            headers.retain(|known, _| !known.eq_ignore_ascii_case(&name));
            headers.insert(name, value);
        }
        options.headers = Some(headers);
        if resolved {
            let mut env = auth.env;
            env.extend(std::mem::take(&mut options.env));
            options.env = env;
        } else {
            options.env.extend(auth.env);
        }
        Ok(())
    }
}
