//! Session-owned provider auth, resolved anew for each physical request.
use std::{collections::BTreeMap, sync::Arc};

use rpi_ai::auth::{
    AuthContext, AuthError, AuthOptions, AuthRuntime, CredentialStore, ProviderAuth, SystemClock,
};
use rpi_ai::{Model, SimpleStreamOptions};

pub(super) struct AuthBinding {
    pub provider: Arc<dyn ProviderAuth>,
    pub runtime: Arc<AuthRuntime>,
    pub store: Result<Arc<dyn CredentialStore>, AuthError>,
    pub context: AuthContext,
    pub operations: Option<Arc<rpi_extensions::provider_runtime::NativeProviderOperations>>,
}

impl AuthBinding {
    pub async fn prepare(
        &self,
        model: &mut Model,
        options: &mut SimpleStreamOptions,
    ) -> Result<(), AuthError> {
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
            } else {
                Some(
                    serde_json::from_value::<rpi_ai::auth::RequestAuth>(result)
                        .map_err(|_| AuthError::Resolve)?,
                )
            }
        } else {
            None
        };
        let auth = match override_auth {
            Some(auth) => auth,
            None => self
                .runtime
                .resolve(
                    model.provider.clone(),
                    self.provider.clone(),
                    self.store.clone()?,
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
        options.api_key = auth.api_key;
        if let Some(base_url) = auth.base_url {
            model.base_url = base_url;
        }
        let headers = options.headers.get_or_insert_with(BTreeMap::new);
        for (name, value) in auth.headers {
            headers.retain(|known, _| !known.eq_ignore_ascii_case(&name));
            headers.insert(name, value);
        }
        options.env.extend(auth.env);
        Ok(())
    }
}
