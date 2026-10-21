use rpi_ai::auth::{
    AuthContext, AuthError, AuthOptions, AuthRuntime, Credential as AuthCredential,
    CredentialStore, CredentialTransaction, OAuthCredential, ProviderAuth, SystemClock,
};
use tokio_util::sync::CancellationToken;

struct FixedCredential(Option<AuthCredential>);
#[async_trait::async_trait]
impl CredentialStore for FixedCredential {
    async fn read(&self, _: &str) -> Result<Option<AuthCredential>, AuthError> {
        Ok(self.0.clone())
    }
    async fn lock(
        &self,
        _: &str,
        _: CancellationToken,
    ) -> Result<Box<dyn CredentialTransaction>, AuthError> {
        Err(AuthError::Store)
    }
}

struct RecordingAuth {
    calls: Mutex<Vec<AuthContext>>,
    base_url: Option<String>,
}
#[async_trait::async_trait]
impl ProviderAuth for RecordingAuth {
    fn supports_api_key(&self) -> bool {
        true
    }
    fn supports_oauth(&self) -> bool {
        false
    }
    async fn resolve_api_key(
        &self,
        context: AuthContext,
        credential: Option<AuthCredential>,
        _: CancellationToken,
    ) -> Result<Option<rpi_ai::auth::RequestAuth>, AuthError> {
        self.calls.lock().unwrap().push(context);
        let Some(AuthCredential::ApiKey {
            key: Some(key),
            env,
        }) = credential
        else {
            return Ok(None);
        };
        Ok(Some(rpi_ai::auth::RequestAuth {
            api_key: Some(key.clone()),
            headers: [("Authorization".into(), format!("Bearer {key}"))].into(),
            env,
            base_url: self.base_url.clone(),
            ..Default::default()
        }))
    }
    async fn refresh(
        &self,
        _: OAuthCredential,
        _: CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        Err(AuthError::Refresh)
    }
    async fn to_auth(&self, _: OAuthCredential) -> Result<rpi_ai::auth::RequestAuth, AuthError> {
        Err(AuthError::Derive)
    }
}

fn fixed_credential(key: &str) -> Arc<dyn CredentialStore> {
    Arc::new(FixedCredential(Some(AuthCredential::ApiKey {
        key: Some(key.into()),
        env: [("ACCOUNT".into(), key.into())].into(),
    })))
}

#[tokio::test]
async fn replacement_drops_default_credentials_env_and_endpoint_without_changing_process_env() {
    let calls = Calls::default();
    let auth = Arc::new(RecordingAuth {
        calls: Mutex::new(vec![]),
        base_url: None,
    });
    let runtime = Arc::new(AuthRuntime::default());
    let mut factories = RuntimeFactories::default();
    factories.protocols.insert(
        "openai-completions".into(),
        Arc::new(RecordingFactory(calls.clone())),
    );
    factories.auth.insert(
        "pool".into(),
        Arc::new(super::super::auth::AuthBinding {
            provider: auth.clone(),
            runtime: runtime.clone(),
            store: Ok(fixed_credential("default-secret")),
            context: AuthContext {
                env: [("DEFAULT_ACCOUNT".into(), "ambient-secret".into())].into(),
            },
        }),
    );
    let canonical = Model::new(
        "model",
        "Model",
        Api::OpenaiCompletions,
        "pool",
        "https://provider.example/v1",
    );
    let mut bound = canonical.clone();
    bound.base_url = "https://default-account.example/v1".into();
    bound.headers = Some([("X-Custom-Credential".into(), "default-secret".into())].into());
    let (provider, _) = factories
        .configure(
            "pool",
            &canonical.api,
            vec![bound],
            std::slice::from_ref(&canonical),
            RequestAuth::ApiKey("bound-secret".into()),
            &settings::Settings::default(),
        )
        .unwrap();
    let before_env = (
        std::env::var_os("ACCOUNT"),
        std::env::var_os("DEFAULT_ACCOUNT"),
    );
    let replacement = runtime
        .resolve_replacement(
            "pool".into(),
            auth.clone(),
            fixed_credential("pooled-secret"),
            AuthOptions::default(),
            CancellationToken::new(),
            Arc::new(SystemClock),
        )
        .await
        .unwrap();
    let options = SimpleStreamOptions {
        api_key: Some("old-option-secret".into()),
        authentication: replacement,
        headers: Some(
            [
                ("authorization".into(), "old-secret".into()),
                ("X-Custom-Credential".into(), "old-custom-secret".into()),
                ("x-trace".into(), "trace".into()),
            ]
            .into(),
        ),
        env: [("DEFAULT_ACCOUNT".into(), "old-env-secret".into())].into(),
        max_retries: Some(0),
        max_tokens: Some(42),
        ..Default::default()
    };
    let mut requested = provider.models()[0].clone();
    requested.base_url = "https://untrusted.example".into();
    provider
        .stream_simple(&requested, &Context::new(vec![]), &options)
        .await
        .result()
        .await
        .unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let (model, opts) = &calls[0];
    assert_eq!(model.base_url, "https://provider.example/v1");
    assert!(model.headers.is_none());
    assert_eq!(opts.api_key.as_deref(), Some("pooled-secret"));
    assert_eq!(
        opts.headers.as_ref().unwrap(),
        &BTreeMap::from([
            ("Authorization".into(), "Bearer pooled-secret".into()),
            ("x-trace".into(), "trace".into())
        ])
    );
    assert_eq!(
        opts.env,
        BTreeMap::from([("ACCOUNT".into(), "pooled-secret".into())])
    );
    assert_eq!(opts.max_tokens, Some(42));
    assert_eq!(opts.max_retries, Some(0));
    assert!(!format!("{opts:?}").contains("secret"));
    assert_eq!(
        auth.calls.lock().unwrap().len(),
        1,
        "replacement must not resolve the default account again"
    );
    assert!(auth.calls.lock().unwrap()[0].env.is_empty());
    assert_eq!(
        (
            std::env::var_os("ACCOUNT"),
            std::env::var_os("DEFAULT_ACCOUNT")
        ),
        before_env
    );
}

#[tokio::test]
async fn provider_owned_auth_is_resolved_per_request_and_unavailable_credentials_fail_closed() {
    let calls = Calls::default();
    let auth = Arc::new(RecordingAuth {
        calls: Mutex::new(vec![]),
        base_url: Some("https://account.example".into()),
    });
    let runtime = Arc::new(AuthRuntime::default());
    let mut factories = RuntimeFactories::default();
    factories.protocols.insert(
        "openai-completions".into(),
        Arc::new(RecordingFactory(calls.clone())),
    );
    factories.auth.insert(
        "pool".into(),
        Arc::new(super::super::auth::AuthBinding {
            provider: auth.clone(),
            runtime: runtime.clone(),
            store: Ok(fixed_credential("account-secret")),
            context: AuthContext::default(),
        }),
    );
    let model = Model::new(
        "model",
        "Model",
        Api::OpenaiCompletions,
        "pool",
        "https://provider.example",
    );
    let (provider, _) = factories
        .configure(
            "pool",
            &model.api,
            vec![model.clone()],
            &[model.clone()],
            RequestAuth::None,
            &settings::Settings::default(),
        )
        .unwrap();
    for _ in 0..2 {
        provider
            .stream_simple(
                &model,
                &Context::new(vec![]),
                &SimpleStreamOptions::default(),
            )
            .await
            .result()
            .await
            .unwrap();
    }
    assert_eq!(auth.calls.lock().unwrap().len(), 2);
    assert!(calls
        .lock()
        .unwrap()
        .iter()
        .all(|(model, opts)| model.base_url == "https://account.example"
            && opts.api_key.as_deref() == Some("account-secret")));
    let unavailable = runtime
        .resolve_replacement(
            "pool".into(),
            auth.clone(),
            Arc::new(FixedCredential(None)),
            AuthOptions::default(),
            CancellationToken::new(),
            Arc::new(SystemClock),
        )
        .await;
    assert!(matches!(unavailable, Err(AuthError::Resolve)));
    let wrong_provider = runtime
        .resolve_replacement(
            "other".into(),
            auth,
            fixed_credential("wrong-secret"),
            AuthOptions::default(),
            CancellationToken::new(),
            Arc::new(SystemClock),
        )
        .await
        .unwrap();
    let error = provider
        .stream_simple(
            &model,
            &Context::new(vec![]),
            &SimpleStreamOptions {
                authentication: wrong_provider,
                ..Default::default()
            },
        )
        .await
        .result()
        .await
        .unwrap();
    assert_eq!(error.stop_reason, StopReason::Error);
    assert_eq!(calls.lock().unwrap().len(), 2);
    runtime.shutdown().await;
    let error = provider
        .stream_simple(
            &model,
            &Context::new(vec![]),
            &SimpleStreamOptions::default(),
        )
        .await
        .result()
        .await
        .unwrap();
    assert_eq!(error.stop_reason, StopReason::Aborted);
    assert_eq!(calls.lock().unwrap().len(), 2);
}
