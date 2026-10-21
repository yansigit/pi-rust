//! An owning provider can narrow request auth without reading or refreshing dormant credentials.
use rpi_ai::auth::{
    AuthContext, AuthError, Credential, CredentialStore, CredentialTransaction, OAuthCredential,
    ProviderAuth, RequestAuth,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Policy {
    pub api_key: bool,
    pub oauth: bool,
    pub fallback_api_key: Option<String>,
}
pub(super) struct Restricted {
    pub inner: Arc<dyn ProviderAuth>,
    pub policy: Policy,
}
#[async_trait::async_trait]
impl ProviderAuth for Restricted {
    fn capture_invocation(&self) -> Result<Option<Arc<dyn ProviderAuth>>, AuthError> {
        Ok(self.inner.capture_invocation()?.map(|inner| {
            Arc::new(Self {
                inner,
                policy: self.policy.clone(),
            }) as Arc<dyn ProviderAuth>
        }))
    }
    fn supports_api_key(&self) -> bool {
        self.policy.api_key && self.inner.supports_api_key()
    }
    fn supports_oauth(&self) -> bool {
        self.policy.oauth && self.inner.supports_oauth()
    }
    async fn resolve_api_key(
        &self,
        context: AuthContext,
        credential: Option<Credential>,
        signal: CancellationToken,
    ) -> Result<Option<RequestAuth>, AuthError> {
        let credential = credential.or_else(|| {
            self.policy
                .fallback_api_key
                .as_ref()
                .map(|key| Credential::ApiKey {
                    key: Some(key.clone()),
                    env: Default::default(),
                })
        });
        self.inner
            .resolve_api_key(context, credential, signal)
            .await
    }
    async fn refresh(
        &self,
        credential: OAuthCredential,
        signal: CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        self.inner.refresh(credential, signal).await
    }
    async fn to_auth(&self, credential: OAuthCredential) -> Result<RequestAuth, AuthError> {
        self.inner.to_auth(credential).await
    }
}

/// An explicit request key is still usable when the dormant store could not be opened.
pub(super) struct UnavailableStore(pub AuthError);
#[async_trait::async_trait]
impl CredentialStore for UnavailableStore {
    async fn read(&self, _: &str) -> Result<Option<Credential>, AuthError> {
        Err(self.0.clone())
    }
    async fn lock(
        &self,
        _: &str,
        _: CancellationToken,
    ) -> Result<Box<dyn CredentialTransaction>, AuthError> {
        Err(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::auth::{AuthOptions, AuthRuntime, SystemClock};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Store {
        credential: Option<Credential>,
        reads: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl CredentialStore for Store {
        async fn read(&self, _: &str) -> Result<Option<Credential>, AuthError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.credential.clone())
        }
        async fn lock(
            &self,
            _: &str,
            _: CancellationToken,
        ) -> Result<Box<dyn CredentialTransaction>, AuthError> {
            panic!("disabled OAuth must never enter a refresh transaction")
        }
    }
    struct Provider;
    #[async_trait::async_trait]
    impl ProviderAuth for Provider {
        fn capture_invocation(&self) -> Result<Option<Arc<dyn ProviderAuth>>, AuthError> {
            Ok(Some(Arc::new(Self)))
        }
        fn supports_api_key(&self) -> bool {
            true
        }
        fn supports_oauth(&self) -> bool {
            true
        }
        async fn resolve_api_key(
            &self,
            context: AuthContext,
            credential: Option<Credential>,
            _: CancellationToken,
        ) -> Result<Option<RequestAuth>, AuthError> {
            let key = match credential {
                Some(Credential::ApiKey { key: Some(key), .. }) if !key.is_empty() => key,
                _ => context.env.get("API_KEY").cloned().unwrap_or_default(),
            };
            Ok((!key.is_empty()).then(|| RequestAuth {
                api_key: Some(key),
                headers: [("X-Value".into(), "provider-header".into())].into(),
                env: [("VALUE".into(), "provider-env".into())].into(),
                ..Default::default()
            }))
        }
        async fn refresh(
            &self,
            _: OAuthCredential,
            _: CancellationToken,
        ) -> Result<OAuthCredential, AuthError> {
            panic!("disabled OAuth refresh")
        }
        async fn to_auth(&self, _: OAuthCredential) -> Result<RequestAuth, AuthError> {
            panic!("disabled OAuth derivation")
        }
    }
    fn policy() -> Arc<dyn ProviderAuth> {
        Arc::new(Restricted {
            inner: Arc::new(Provider),
            policy: Policy {
                api_key: true,
                oauth: false,
                fallback_api_key: Some("marker".into()),
            },
        })
    }
    #[tokio::test]
    async fn request_policy_preserves_stored_and_explicit_key_precedence_without_oauth_fallback() {
        let runtime = AuthRuntime::default();
        for credential in [
            None,
            Some(Credential::ApiKey {
                key: Some("stored".into()),
                env: Default::default(),
            }),
            Some(Credential::Oauth(OAuthCredential {
                access: "dormant".into(),
                refresh: "do-not-refresh".into(),
                expires_ms: 1,
                extra: Default::default(),
            })),
        ] {
            for explicit in [None, Some("explicit".to_owned()), Some(String::new())] {
                let expected = match (&explicit, &credential) {
                    (Some(key), _) => Some(if key.is_empty() { "ambient" } else { key }),
                    (None, None) => Some("marker"),
                    (None, Some(Credential::ApiKey { .. })) => Some("stored"),
                    _ => None,
                };
                let store = Arc::new(Store {
                    credential: credential.clone(),
                    reads: AtomicUsize::new(0),
                });
                let value = runtime
                    .resolve(
                        "provider".into(),
                        policy(),
                        store.clone(),
                        AuthContext {
                            env: [("API_KEY".into(), "ambient".into())].into(),
                        },
                        AuthOptions {
                            api_key: explicit.clone(),
                            ..Default::default()
                        },
                        CancellationToken::new(),
                        Arc::new(SystemClock),
                    )
                    .await
                    .unwrap();
                assert_eq!(value.as_ref().and_then(|v| v.api_key.as_deref()), expected);
                assert_eq!(
                    store.reads.load(Ordering::SeqCst),
                    usize::from(explicit.is_none())
                );
            }
        }
    }
    #[tokio::test]
    async fn explicit_empty_key_headers_and_environment_keep_pi_request_precedence() {
        let binding = super::super::auth::AuthBinding {
            model_headers: Default::default(),
            credential_headers: Default::default(),
            provider: Arc::new(Provider),
            runtime: Arc::new(AuthRuntime::default()),
            store: Err(AuthError::Store),
            context: AuthContext {
                env: [("API_KEY".into(), "ambient".into())].into(),
            },
            operations: None,
        };
        let mut model = rpi_ai::Model::new(
            "model",
            "Model",
            rpi_ai::Api::Faux,
            "provider",
            "https://example.invalid",
        );
        let mut options = rpi_ai::SimpleStreamOptions {
            api_key: Some(String::new()),
            headers: Some([("x-value".into(), "request-header".into())].into()),
            env: [("VALUE".into(), "request-env".into())].into(),
            ..Default::default()
        };
        binding.prepare(&mut model, &mut options).await.unwrap();
        assert_eq!(options.api_key.as_deref(), Some(""));
        assert_eq!(
            options.headers.as_ref().unwrap(),
            &[("x-value".into(), "request-header".into())].into()
        );
        assert_eq!(options.env["VALUE"], "request-env");
        options.api_key = None;
        assert_eq!(
            binding.prepare(&mut model, &mut options).await.unwrap_err(),
            AuthError::Store
        );
    }
}
