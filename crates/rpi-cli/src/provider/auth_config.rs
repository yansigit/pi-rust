//! Private provider configuration is composed only for ambient native authentication.
use crate::config::{native_values, ProviderConfig};
use rpi_ai::auth::{
    AuthContext, AuthError, Credential, OAuthCredential, ProviderAuth, RequestAuth,
};
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;

pub(crate) struct Configured {
    pub id: String,
    pub inner: Arc<dyn ProviderAuth>,
    pub config: ProviderConfig,
    pub environment: BTreeMap<String, String>,
}
impl Configured {
    async fn headers(
        &self,
        mut auth: RequestAuth,
        env: BTreeMap<String, String>,
    ) -> Result<RequestAuth, AuthError> {
        for (name, raw) in self.config.headers.iter().flatten() {
            let value = native_values::resolve(
                raw,
                &format!("provider {:?} header {name:?}", self.id),
                &env,
            )
            .await
            .map_err(AuthError::Provider)?;
            auth.headers.insert(name.clone(), value);
        }
        if self.config.auth_header == Some(true) {
            let key = auth
                .api_key
                .as_ref()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    AuthError::Provider("authHeader requires a resolved API key".into())
                })?;
            auth.headers
                .insert("Authorization".into(), format!("Bearer {key}"));
        }
        Ok(auth)
    }
}
#[async_trait::async_trait]
impl ProviderAuth for Configured {
    fn capture_invocation(&self) -> Result<Option<Arc<dyn ProviderAuth>>, AuthError> {
        Ok(self.inner.capture_invocation()?.map(|inner| {
            Arc::new(Self {
                inner,
                id: self.id.clone(),
                config: self.config.clone(),
                environment: self.environment.clone(),
            }) as Arc<dyn ProviderAuth>
        }))
    }
    fn supports_api_key(&self) -> bool {
        self.inner.supports_api_key()
    }
    fn supports_oauth(&self) -> bool {
        self.inner.supports_oauth()
    }
    async fn resolve_api_key(
        &self,
        context: AuthContext,
        mut credential: Option<Credential>,
        signal: CancellationToken,
    ) -> Result<Option<RequestAuth>, AuthError> {
        let environment = context.env.clone();
        let mut explicit = match &credential {
            Some(Credential::ApiKey { env, .. }) => env.clone(),
            _ => BTreeMap::new(),
        };
        if credential.is_none() {
            if let Some(raw) = &self.config.api_key {
                let key = native_values::resolve(
                    raw,
                    &format!("API key for provider {:?}", self.id),
                    &environment,
                )
                .await
                .map_err(AuthError::Provider)?;
                credential = Some(Credential::ApiKey {
                    key: Some(key),
                    env: BTreeMap::new(),
                });
            }
        }
        let Some(auth) = self
            .inner
            .resolve_api_key(context, credential, signal)
            .await?
        else {
            return Ok(None);
        };
        explicit.extend(auth.env.clone());
        self.headers(auth, native_values::overlay(&environment, &explicit))
            .await
            .map(Some)
    }
    async fn refresh(
        &self,
        credential: OAuthCredential,
        signal: CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        self.inner.refresh(credential, signal).await
    }
    async fn to_auth(&self, credential: OAuthCredential) -> Result<RequestAuth, AuthError> {
        let env = credential
            .extra
            .get("env")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default();
        let auth = self.inner.to_auth(credential).await?;
        self.headers(auth, native_values::overlay(&self.environment, &env))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::Mutex;
    struct Fixture(Arc<Mutex<Vec<Value>>>);
    #[async_trait::async_trait]
    impl ProviderAuth for Fixture {
        fn capture_invocation(&self) -> Result<Option<Arc<dyn ProviderAuth>>, AuthError> {
            Ok(Some(Arc::new(Self(self.0.clone()))))
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
            self.0
                .lock()
                .unwrap()
                .push(json!({"credential":credential}));
            let (key, env) = match credential {
                Some(Credential::ApiKey { key, env }) => (key, env),
                _ => (None, BTreeMap::new()),
            };
            let stored = key.as_ref().is_some_and(|key| !key.is_empty());
            let key = key
                .filter(|key| !key.is_empty())
                .or_else(|| context.env.get("RPI_FIXTURE_AMBIENT").cloned());
            Ok(key.map(|key| RequestAuth {
                api_key: Some(key),
                headers: [("X-Base".into(), "base-header".into())].into(),
                env,
                source: Some(
                    if stored {
                        "stored credential"
                    } else {
                        "ambient"
                    }
                    .into(),
                ),
                ..Default::default()
            }))
        }
        async fn refresh(
            &self,
            credential: OAuthCredential,
            _: CancellationToken,
        ) -> Result<OAuthCredential, AuthError> {
            Ok(credential)
        }
        async fn to_auth(&self, credential: OAuthCredential) -> Result<RequestAuth, AuthError> {
            self.0
                .lock()
                .unwrap()
                .push(json!({"oauth":credential.access}));
            Ok(RequestAuth {
                api_key: Some(credential.access),
                headers: [("X-Base".into(), "oauth-base".into())].into(),
                ..Default::default()
            })
        }
    }
    #[tokio::test]
    async fn configured_keys_headers_and_oauth_match_pi_1_3_composition() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../../docs/provider-parity/configured-auth-reference.json"
        ))
        .unwrap();
        for (index, case) in cases["cases"].as_array().unwrap().iter().enumerate() {
            let spec = &case["spec"];
            let fixture = Arc::new(Fixture(Arc::new(Mutex::new(vec![]))));
            let mut environment: BTreeMap<String, String> =
                serde_json::from_value(spec["environment"].clone()).unwrap();
            environment.insert("RPI_FIXTURE_AMBIENT".into(), "ambient-key".into());
            let provider=Configured {id:"fixture".into(),inner:fixture.clone(),config:serde_json::from_value(json!({"apiKey":spec["configuredKey"],"headers":spec["configuredHeaders"],"authHeader":spec["authHeader"]})).unwrap(),environment:environment.clone()};
            let provider = provider.capture_invocation().unwrap().unwrap();
            let credential: Option<Credential> = spec
                .get("credential")
                .map(|value| serde_json::from_value(value.clone()).unwrap());
            let oauth = matches!(credential, Some(Credential::Oauth(_)));
            let actual = match credential {
                Some(Credential::Oauth(credential)) => provider.to_auth(credential).await.map(Some),
                credential => {
                    provider
                        .resolve_api_key(
                            AuthContext { env: environment },
                            credential,
                            CancellationToken::new(),
                        )
                        .await
                }
            };
            if let Some(error) = case["result"].get("error") {
                assert_eq!(
                    actual.err().unwrap().to_string(),
                    error.as_str().unwrap(),
                    "case {index}"
                );
            } else {
                let actual = actual.unwrap().unwrap();
                let expected = &case["result"]["value"];
                let expected_auth = if oauth { expected } else { &expected["auth"] };
                assert_eq!(
                    actual.api_key.as_deref(),
                    expected_auth["apiKey"].as_str(),
                    "key, case {index}"
                );
                assert_eq!(
                    json!(actual.headers),
                    expected_auth["headers"],
                    "headers, case {index}"
                );
                if !oauth {
                    assert_eq!(
                        json!(actual.env),
                        expected.get("env").cloned().unwrap_or_else(|| json!({})),
                        "env, case {index}"
                    );
                    assert_eq!(
                        actual.source.as_deref(),
                        expected["source"].as_str(),
                        "source, case {index}"
                    );
                }
            }
            assert_eq!(
                json!(*fixture.0.lock().unwrap()),
                case["calls"],
                "calls, case {index}"
            );
        }
    }
}
