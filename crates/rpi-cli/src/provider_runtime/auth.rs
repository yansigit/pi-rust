//! Resumable logins bind the selected destination before asking for user input.
use super::*;
use rpi_ai::auth::{AuthContext, AuthOptions, Clock, Credential, CredentialStore, SystemClock};
use rpi_extensions::provider_auth::{
    AuthAction, AuthInteraction, AuthMethod, AuthResponseKind, ProviderAuthentication,
};

pub(super) struct Login {
    owner: NativeOwner,
    auth: Arc<ProviderAuthentication>,
    store: Arc<dyn CredentialStore>,
    continuation: Value,
}
impl ProviderRuntime {
    async fn registered_auth(&self, id: &str) -> Result<Arc<ProviderAuthentication>, String> {
        if !self.state.lock().await.catalog.has_provider(id) {
            return Err("provider is not registered".into());
        }
        Ok(ProviderAuthentication::for_registered_provider(
            &self.session,
            id,
        ))
    }
    fn credential_store(
        &self,
        owner: &NativeOwner,
        provider: &str,
        args: &Value,
    ) -> Result<Arc<dyn CredentialStore>, String> {
        if let Some(store_id) = args.get("storeId") {
            let id = store_id.as_str().ok_or("invalid credential store")?;
            let account = args["accountId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("missing account identity")?;
            if !self.session.snapshot().is_some_and(|s| {
                s.credential_stores()
                    .iter()
                    .any(|store| store.manifest.id == id && store.plugin == owner.plugin)
            }) {
                return Err("credential store belongs to another plugin".into());
            }
            let store = rpi_extensions::credential_store::NativeCredentialStore::from_session(
                &self.session,
                id,
                provider,
                account,
            )
            .ok_or("credential store unavailable")?;
            Ok(Arc::new(
                rpi_extensions::credential_store::NativeStoreHandle(store),
            ))
        } else {
            let path = crate::config::auth_path().map_err(|_| "credential storage unavailable")?;
            Ok(Arc::new(
                crate::config::auth_store::CompatibilityCredentialStore::new(path)
                    .map_err(|e| e.to_string())?,
            ))
        }
    }
    async fn auth_methods(&self, id: &str, signal: CancellationToken) -> Result<Value, String> {
        let auth = self.registered_auth(id).await?;
        if let ProviderAuthentication::Native(auth) = auth.as_ref() {
            return auth
                .methods(signal)
                .await
                .map(|methods| json!(methods))
                .map_err(|e| e.to_string());
        }
        if let Some(operations) =
            rpi_extensions::provider_runtime::NativeProviderOperations::from_session(
                &self.session,
                id,
            )
        {
            if operations.supports("authMethods") {
                let result = operations.request("authMethods", json!({}), signal).await?;
                let mut manifest = auth.manifest().clone();
                manifest.methods = serde_json::from_value(result)
                    .map_err(|_| "invalid provider authentication methods")?;
                manifest
                    .validate()
                    .map_err(|_| "invalid provider authentication methods")?;
                return Ok(json!(manifest.methods));
            }
        }
        Ok(json!(auth.manifest().methods))
    }
    pub(super) async fn auth_request(
        &self,
        owner: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        self.validate_owner(&owner)?;
        if signal.is_cancelled() {
            return Err("authentication cancelled".into());
        }
        let action = args["action"]
            .as_str()
            .ok_or("missing authentication action")?;
        if action == "continue" || action == "cancelLogin" {
            let ticket = args["loginId"].as_str().ok_or("missing login identity")?;
            let mut logins = self.logins.lock().await;
            if !logins.get(ticket).is_some_and(|login| login.owner == owner) {
                return Err("login belongs to another session or plugin".into());
            }
            let login = logins.remove(ticket).expect("checked login");
            drop(logins);
            if action == "cancelLogin" {
                return Ok(json!({"kind":"cancelled"}));
            }
            let input = json!({"continuation":login.continuation,"value":args["value"]});
            return self
                .login_step(login, AuthAction::Continue, input, signal)
                .await;
        }
        let id = args["providerId"]
            .as_str()
            .ok_or("missing provider identity")?;
        if action == "credentialStore" {
            return self
                .credential_store_request(owner, id, &args, signal)
                .await;
        }
        if action == "methods" {
            return self.auth_methods(id, signal).await;
        }
        if action == "resolve" {
            return self.resolve_owned_auth(owner, id, &args, signal).await;
        }
        let auth = self.registered_auth(id).await?;
        let store = self.credential_store(&owner, id, &args)?;
        match action {
            "login" => {
                let method = args["methodId"]
                    .as_str()
                    .ok_or("missing authentication method")?;
                let methods: Vec<AuthMethod> =
                    serde_json::from_value(self.auth_methods(id, signal.clone()).await?)
                        .map_err(|_| "invalid authentication methods")?;
                if !methods.iter().any(|known| known.id == method) {
                    return Err("authentication method is unavailable".into());
                }
                self.login_step(
                    Login {
                        owner,
                        auth,
                        store,
                        continuation: Value::Null,
                    },
                    AuthAction::Login,
                    json!({"methodId":method}),
                    signal,
                )
                .await
            }
            "status" => {
                // Presence only: availability must never execute a `!command`
                // credential (pi `auth-storage.js:213-214`).
                let credential = store.read_unresolved(id).await.map_err(|e| e.to_string())?;
                Ok(match credential {
                    Some(Credential::Oauth(credential)) => {
                        json!({"present":true,"type":"oauth","expires":credential.expires_ms,"refreshRequired":credential.expires_ms <= SystemClock.now_ms().saturating_add(rpi_ai::auth::MINIMUM_OAUTH_VALIDITY_MS)})
                    }
                    Some(Credential::ApiKey { .. }) => json!({"present":true,"type":"api_key"}),
                    None => json!({"present":false}),
                })
            }
            "refresh" => {
                let options = AuthOptions {
                    force_refresh: true,
                    ..Default::default()
                };
                if args.get("storeId").is_some() {
                    self.session
                        .auth_runtime()
                        .resolve_replacement(
                            id.into(),
                            auth,
                            store,
                            options,
                            signal,
                            Arc::new(SystemClock),
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                } else {
                    self.session
                        .auth_runtime()
                        .resolve(
                            id.into(),
                            auth,
                            store,
                            AuthContext {
                                env: std::env::vars_os()
                                    .filter_map(|(k, v)| {
                                        Some((k.into_string().ok()?, v.into_string().ok()?))
                                    })
                                    .collect(),
                            },
                            options,
                            signal,
                            Arc::new(SystemClock),
                        )
                        .await
                        .map_err(|e| e.to_string())?
                        .ok_or("authentication unavailable")?;
                }
                Ok(json!({"kind":"success"}))
            }
            "logout" => {
                let mut transaction = store
                    .lock(id, signal.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                if let Some(credential) = transaction.current() {
                    let response = auth
                        .request(AuthAction::Revoke, json!({"credential":credential}), signal)
                        .await
                        .map_err(|e| e.to_string())?;
                    if response.kind != AuthResponseKind::Success {
                        return Err("provider did not acknowledge logout".into());
                    }
                    transaction.remove().await.map_err(|e| e.to_string())?;
                }
                Ok(json!({"kind":"success"}))
            }
            _ => Err("unsupported authentication action".into()),
        }
    }

    async fn resolve_owned_auth(
        &self,
        owner: NativeOwner,
        id: &str,
        args: &Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let registered = self.state.lock().await.catalog.owner(id);
        if !registered.is_some_and(|registered| {
            registered.generation == owner.generation
                && registered.plugin == owner.plugin
                && registered.id == owner.id
        }) {
            return Err("resolved authentication belongs to another session or plugin".into());
        }
        if args.as_object().map_or(true, |fields| {
            fields.keys().any(|key| {
                !["version", "port", "action", "providerId", "env"].contains(&key.as_str())
            })
        }) {
            return Err("unsupported resolved authentication field".into());
        }
        let env = args
            .get("env")
            .map_or_else(
                || Ok(BTreeMap::new()),
                |value| serde_json::from_value::<BTreeMap<String, String>>(value.clone()),
            )
            .map_err(|_| "invalid request authentication environment")?;
        let binding = self
            .auth
            .get(id)
            .ok_or("provider has no native authentication")?;
        let auth = binding
            .runtime
            .resolve(
                id.into(),
                binding.provider.clone(),
                binding.store.clone().map_err(|error| error.to_string())?,
                binding.context.clone(),
                AuthOptions {
                    env,
                    ..Default::default()
                },
                signal,
                Arc::new(SystemClock),
            )
            .await
            .map_err(|error| error.to_string())?;
        serde_json::to_value(auth).map_err(|_| "invalid resolved authentication".into())
    }

    async fn credential_store_request(
        &self,
        owner: NativeOwner,
        id: &str,
        args: &Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        if owner.child || self.state.lock().await.catalog.owner(id).as_ref() != Some(&owner) {
            return Err("credential store callback belongs to another session or plugin".into());
        }
        if args.get("storeId").is_some() || args.get("path").is_some() {
            return Err("native credential storage is selected by the host".into());
        }
        let operations = rpi_extensions::provider_runtime::NativeProviderOperations::from_session(
            &self.session,
            id,
        )
        .filter(|operations| operations.supports("credentialStore"))
        .ok_or("provider has no credential store callback")?;
        let timeout = args.get("lockTimeoutMs").map_or(Ok(30_000), |value| {
            value
                .as_u64()
                .filter(|ms| *ms <= 30_000)
                .ok_or("invalid credential lock timeout")
        })?;
        let path = crate::config::auth_path().map_err(|_| "credential storage unavailable")?;
        let store = crate::config::auth_store::CompatibilityCredentialStore::new(path)
            .map_err(|e| e.to_string())?;
        let input = args["input"].clone();
        let snapshot = self
            .session
            .snapshot_arc()
            .ok_or("provider session is unavailable")?;
        self.session
            .auth_runtime()
            .run_owned(signal, move |admission| async move {
                let lease = store
                    .lock_file(admission.clone(), std::time::Duration::from_millis(timeout))
                    .await?;
                if admission.is_cancelled() || !snapshot.is_active() {
                    return Err(rpi_ai::auth::AuthError::Cancelled);
                }
                operations
                    .credential_store(
                        json!({"authPath":lease.path(),"expiryUnit":"seconds","input":input}),
                    )
                    .await
                    .map_err(rpi_ai::auth::AuthError::Provider)
            })
            .await
            .map_err(|e| e.to_string())
    }
    async fn login_step(
        &self,
        mut login: Login,
        action: AuthAction,
        input: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let response = login
            .auth
            .request(action, input, signal.clone())
            .await
            .map_err(|e| e.to_string())?;
        match response.kind {
            AuthResponseKind::Credential => {
                let credential: Credential = serde_json::from_value(response.payload)
                    .map_err(|_| "invalid login credential")?;
                if !matches!(&credential, Credential::Oauth(_) if login.auth.manifest().oauth)
                    && !matches!(&credential, Credential::ApiKey { .. } if login.auth.manifest().api_key)
                {
                    return Err("unsupported login credential".into());
                }
                self.validate_owner(&login.owner)?;
                let mut transaction = login
                    .store
                    .lock(&login.auth.manifest().id, signal)
                    .await
                    .map_err(|e| e.to_string())?;
                transaction
                    .replace(credential)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(json!({"kind":"success"}))
            }
            AuthResponseKind::Interaction => {
                AuthInteraction::parse(response.payload.clone()).map_err(|e| e.to_string())?;
                let mut presentation = response.payload;
                login.continuation = presentation
                    .as_object_mut()
                    .expect("validated interaction")
                    .remove("continuation")
                    .expect("validated continuation");
                self.validate_owner(&login.owner)?;
                let mut logins = self.logins.lock().await;
                if logins.len() >= 256 {
                    return Err("too many pending authentication interactions".into());
                }
                let ticket = uuid::Uuid::new_v4().to_string();
                logins.insert(ticket.clone(), login);
                Ok(json!({"kind":"interaction","loginId":ticket,"interaction":presentation}))
            }
            _ => Err("provider returned an invalid login response".into()),
        }
    }
}
