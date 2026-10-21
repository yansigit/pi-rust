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
}

impl AuthBinding {
    pub async fn prepare(
        &self,
        model: &mut Model,
        options: &mut SimpleStreamOptions,
    ) -> Result<(), AuthError> {
        let auth = self
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
            .ok_or(AuthError::Resolve)?;
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
