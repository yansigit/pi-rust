use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};

async fn resolve(
    id: String,
    provider: Arc<dyn ProviderAuth>,
    store: Arc<dyn CredentialStore>,
    context: AuthContext,
    options: AuthOptions,
    signal: CancellationToken,
    clock: Arc<dyn Clock>,
) -> Result<Option<RequestAuth>, AuthError> {
    AuthRuntime::default()
        .resolve(id, provider, store, context, options, signal, clock)
        .await
}

struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        1_000_000
    }
}

fn oauth(expires_ms: i64) -> OAuthCredential {
    OAuthCredential {
        access: "old-access".into(),
        refresh: "old-refresh".into(),
        expires_ms,
        extra: BTreeMap::new(),
    }
}

struct MemoryStore(Arc<Mutex<Option<Credential>>>, Arc<AtomicUsize>);
struct Transaction(OwnedMutexGuard<Option<Credential>>, Arc<AtomicUsize>);

#[async_trait::async_trait]
impl CredentialTransaction for Transaction {
    fn current(&self) -> Option<Credential> {
        self.0.clone()
    }
    async fn replace(&mut self, credential: Credential) -> Result<(), AuthError> {
        *self.0 = Some(credential);
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl CredentialStore for MemoryStore {
    async fn read(&self, _: &str) -> Result<Option<Credential>, AuthError> {
        Ok(self.0.lock().await.clone())
    }
    async fn lock(
        &self,
        _: &str,
        signal: CancellationToken,
    ) -> Result<Box<dyn CredentialTransaction>, AuthError> {
        tokio::select! {
            biased;
            _ = signal.cancelled() => Err(AuthError::Cancelled),
            guard = self.0.clone().lock_owned() => Ok(Box::new(Transaction(guard, self.1.clone())) as Box<dyn CredentialTransaction>),
        }
    }
}

struct Auth {
    refreshes: AtomicUsize,
    resolutions: AtomicUsize,
    entered: Semaphore,
    release: Semaphore,
    expires_ms: i64,
    fail: bool,
    oauth_supported: bool,
    observed: Mutex<Option<(AuthContext, Option<Credential>)>>,
}

impl Auth {
    fn new(permits: usize) -> Self {
        Self {
            refreshes: AtomicUsize::new(0),
            resolutions: AtomicUsize::new(0),
            entered: Semaphore::new(0),
            release: Semaphore::new(permits),
            expires_ms: 2_000_000,
            fail: false,
            oauth_supported: true,
            observed: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl ProviderAuth for Auth {
    fn supports_api_key(&self) -> bool {
        true
    }
    fn supports_oauth(&self) -> bool {
        self.oauth_supported
    }
    async fn resolve_api_key(
        &self,
        context: AuthContext,
        credential: Option<Credential>,
        _: CancellationToken,
    ) -> Result<Option<RequestAuth>, AuthError> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        *self.observed.lock().await = Some((context, credential));
        Ok(Some(RequestAuth::default()))
    }
    async fn refresh(
        &self,
        _: OAuthCredential,
        signal: CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        tokio::select! {
            _ = signal.cancelled() => return Err(AuthError::Cancelled),
            permit = self.release.acquire() => permit.unwrap().forget(),
        }
        if self.fail {
            return Err(AuthError::Refresh);
        }
        Ok(OAuthCredential {
            access: "new-access".into(),
            refresh: "rotated-refresh".into(),
            ..oauth(self.expires_ms)
        })
    }
    async fn to_auth(&self, credential: OAuthCredential) -> Result<RequestAuth, AuthError> {
        Ok(RequestAuth {
            api_key: Some(credential.access),
            ..RequestAuth::default()
        })
    }
}

fn store(credential: Option<Credential>) -> Arc<MemoryStore> {
    Arc::new(MemoryStore(
        Arc::new(Mutex::new(credential)),
        Arc::new(AtomicUsize::new(0)),
    ))
}

async fn run(
    auth: Arc<Auth>,
    store: Arc<MemoryStore>,
    signal: CancellationToken,
    options: AuthOptions,
) -> Result<Option<RequestAuth>, AuthError> {
    resolve(
        "test".into(),
        auth,
        store,
        AuthContext::default(),
        options,
        signal,
        Arc::new(FixedClock),
    )
    .await
}

#[tokio::test]
async fn five_minute_boundary_and_concurrent_callers_refresh_once() {
    let auth = Arc::new(Auth::new(1));
    let store = store(Some(Credential::Oauth(oauth(1_300_000))));
    let first = run(
        auth.clone(),
        store.clone(),
        CancellationToken::new(),
        AuthOptions::default(),
    );
    let second = run(
        auth.clone(),
        store.clone(),
        CancellationToken::new(),
        AuthOptions::default(),
    );
    let (first, second) = tokio::join!(first, second);
    assert_eq!(
        first.unwrap().unwrap().api_key.as_deref(),
        Some("new-access")
    );
    assert_eq!(
        second.unwrap().unwrap().api_key.as_deref(),
        Some("new-access")
    );
    assert_eq!(auth.refreshes.load(Ordering::SeqCst), 1);
    assert!(
        matches!(store.read("test").await.unwrap(), Some(Credential::Oauth(credential)) if credential.refresh == "rotated-refresh")
    );
}

#[tokio::test]
async fn an_explicit_refresh_rotates_a_still_valid_token_and_commits_before_returning() {
    let auth = Arc::new(Auth::new(1));
    let store = store(Some(Credential::Oauth(oauth(9_000_000))));
    run(
        auth.clone(),
        store.clone(),
        CancellationToken::new(),
        AuthOptions {
            force_refresh: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(auth.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(store.1.load(Ordering::SeqCst), 1);
    assert!(
        matches!(store.read("test").await.unwrap(), Some(Credential::Oauth(credential)) if credential.refresh == "rotated-refresh")
    );
}

#[tokio::test]
async fn sufficient_validity_does_not_refresh() {
    let auth = Arc::new(Auth::new(0));
    let store = store(Some(Credential::Oauth(oauth(1_300_001))));
    let result = run(
        auth.clone(),
        store,
        CancellationToken::new(),
        AuthOptions::default(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.api_key.as_deref(), Some("old-access"));
    assert_eq!(auth.refreshes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_caller_does_not_discard_rotated_token_or_provider() {
    let auth = Arc::new(Auth::new(0));
    let weak = Arc::downgrade(&auth);
    let store = store(Some(Credential::Oauth(oauth(1))));
    let signal = CancellationToken::new();
    let request = tokio::spawn(run(
        auth.clone(),
        store.clone(),
        signal.clone(),
        AuthOptions::default(),
    ));
    auth.entered.acquire().await.unwrap().forget();
    signal.cancel();
    assert!(matches!(request.await.unwrap(), Err(AuthError::Cancelled)));
    assert!(
        store.0.try_lock().is_err(),
        "refresh still holds the store transaction"
    );
    auth.release.add_permits(1);
    drop(auth);
    let persisted = store.0.lock().await;
    assert!(
        matches!(&*persisted, Some(Credential::Oauth(credential)) if credential.refresh == "rotated-refresh")
    );
    drop(persisted);
    tokio::task::yield_now().await;
    assert!(
        weak.upgrade().is_none(),
        "provider is released after completion"
    );
}

#[tokio::test(start_paused = true)]
async fn refresh_timeout_preserves_stored_credential() {
    let auth = Arc::new(Auth::new(0));
    let original = Credential::Oauth(oauth(1));
    let store = store(Some(original.clone()));
    let request = tokio::spawn(run(
        auth.clone(),
        store.clone(),
        CancellationToken::new(),
        AuthOptions::default(),
    ));
    auth.entered.acquire().await.unwrap().forget();
    tokio::time::advance(OAUTH_REFRESH_TIMEOUT).await;
    assert!(matches!(
        request.await.unwrap(),
        Err(AuthError::RefreshTimeout)
    ));
    assert!(store.read("test").await.unwrap() == Some(original));
    assert_eq!(auth.resolutions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unsupported_stored_type_and_failed_refresh_never_fall_back() {
    for oauth_supported in [false, true] {
        let auth = Arc::new(Auth {
            fail: true,
            oauth_supported,
            ..Auth::new(1)
        });
        let result = run(
            auth.clone(),
            store(Some(Credential::Oauth(oauth(1)))),
            CancellationToken::new(),
            AuthOptions::default(),
        )
        .await;
        if oauth_supported {
            assert!(matches!(result, Err(AuthError::Refresh)));
        } else {
            assert!(matches!(result, Ok(None)));
        }
        assert_eq!(auth.resolutions.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn short_refreshed_token_is_persisted_and_only_explicit_minimum_rejects_it() {
    for minimum_validity_ms in [None, Some(0), Some(400_000)] {
        let auth = Arc::new(Auth {
            expires_ms: 1_100_000,
            ..Auth::new(1)
        });
        let store = store(Some(Credential::Oauth(oauth(1))));
        let result = run(
            auth,
            store.clone(),
            CancellationToken::new(),
            AuthOptions {
                minimum_validity_ms,
                ..AuthOptions::default()
            },
        )
        .await;
        if minimum_validity_ms.is_some() {
            assert!(matches!(result, Err(AuthError::InsufficientValidity)));
        } else {
            assert!(result.is_ok());
        }
        assert!(
            matches!(store.read("test").await.unwrap(), Some(Credential::Oauth(credential)) if credential.refresh == "rotated-refresh")
        );
    }
}

#[tokio::test]
async fn explicit_key_and_environment_are_request_local() {
    let auth = Arc::new(Auth::new(0));
    let context = AuthContext {
        env: BTreeMap::from([
            ("REGION".into(), "ambient".into()),
            ("EMPTY".into(), "fallback".into()),
        ]),
    };
    let options = AuthOptions {
        api_key: Some("replacement".into()),
        env: BTreeMap::from([
            ("REGION".into(), "account".into()),
            ("EMPTY".into(), String::new()),
        ]),
        ..AuthOptions::default()
    };
    resolve(
        "test".into(),
        auth.clone(),
        store(Some(Credential::Oauth(oauth(1)))),
        context.clone(),
        options,
        CancellationToken::new(),
        Arc::new(FixedClock),
    )
    .await
    .unwrap();
    let observed = auth.observed.lock().await;
    let (effective, credential) = observed.as_ref().unwrap();
    assert_eq!(effective.env["REGION"], "account");
    assert_eq!(effective.env["EMPTY"], "fallback");
    assert_eq!(context.env["REGION"], "ambient");
    assert!(
        matches!(credential, Some(Credential::ApiKey { key: Some(key), env }) if key == "replacement" && env["EMPTY"].is_empty())
    );
    assert_eq!(auth.refreshes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn already_cancelled_request_does_no_auth_work() {
    let auth = Arc::new(Auth::new(0));
    let signal = CancellationToken::new();
    signal.cancel();
    assert!(matches!(
        run(auth.clone(), store(None), signal, AuthOptions::default()).await,
        Err(AuthError::Cancelled)
    ));
    assert_eq!(auth.resolutions.load(Ordering::SeqCst), 0);
}

struct StaleReadStore(Arc<MemoryStore>);

#[async_trait::async_trait]
impl CredentialStore for StaleReadStore {
    async fn read(&self, _: &str) -> Result<Option<Credential>, AuthError> {
        Ok(Some(Credential::Oauth(oauth(1))))
    }

    async fn lock(
        &self,
        provider: &str,
        signal: CancellationToken,
    ) -> Result<Box<dyn CredentialTransaction>, AuthError> {
        self.0.lock(provider, signal).await
    }
}

#[tokio::test]
async fn authoritative_lock_read_observes_refresh_logout_and_changed_credential_type() {
    for current in [
        Some(Credential::Oauth(oauth(2_000_000))),
        None,
        Some(Credential::ApiKey {
            key: Some("changed".into()),
            env: BTreeMap::new(),
        }),
    ] {
        let is_oauth = matches!(current, Some(Credential::Oauth(_)));
        let auth = Arc::new(Auth::new(0));
        let result = resolve(
            "test".into(),
            auth.clone(),
            Arc::new(StaleReadStore(store(current))),
            AuthContext::default(),
            AuthOptions::default(),
            CancellationToken::new(),
            Arc::new(FixedClock),
        )
        .await
        .unwrap();
        assert_eq!(result.is_some(), is_oauth);
        assert_eq!(auth.refreshes.load(Ordering::SeqCst), 0);
        assert_eq!(auth.resolutions.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn shutdown_waits_for_rotated_token_and_refuses_new_work() {
    let runtime = Arc::new(AuthRuntime::default());
    let auth = Arc::new(Auth::new(0));
    let store = store(Some(Credential::Oauth(oauth(1))));
    let request = tokio::spawn({
        let (runtime, auth, store) = (runtime.clone(), auth.clone(), store.clone());
        async move {
            runtime
                .resolve(
                    "test".into(),
                    auth,
                    store,
                    AuthContext::default(),
                    AuthOptions::default(),
                    CancellationToken::new(),
                    Arc::new(FixedClock),
                )
                .await
        }
    });
    auth.entered.acquire().await.unwrap().forget();
    let shutdown = runtime.shutdown();
    tokio::pin!(shutdown);
    assert!(futures::poll!(&mut shutdown).is_pending());
    assert!(matches!(request.await.unwrap(), Err(AuthError::Cancelled)));
    let rejected = runtime
        .resolve(
            "test".into(),
            auth.clone(),
            store.clone(),
            AuthContext::default(),
            AuthOptions::default(),
            CancellationToken::new(),
            Arc::new(FixedClock),
        )
        .await;
    assert!(matches!(rejected, Err(AuthError::Cancelled)));
    auth.release.add_permits(1);
    shutdown.await;
    runtime.shutdown().await;
    assert!(
        matches!(store.read("test").await.unwrap(), Some(Credential::Oauth(credential)) if credential.refresh == "rotated-refresh")
    );
    assert_eq!(auth.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn matches_executed_pi_130_auth_resolution_fixtures() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/auth-pi-1.3.0.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let runtime = AuthRuntime::default();
        let auth = Arc::new(Auth {
            expires_ms: case["refreshedExpiry"].as_i64().unwrap(),
            ..Auth::new(1)
        });
        let store = store(Some(Credential::Oauth(oauth(
            case["expires"].as_i64().unwrap(),
        ))));
        let requests = (0..case["requests"].as_u64().unwrap()).map(|_| {
            runtime.resolve(
                "test".into(),
                auth.clone(),
                store.clone(),
                AuthContext::default(),
                AuthOptions::default(),
                CancellationToken::new(),
                Arc::new(FixedClock),
            )
        });
        let results: Vec<_> = futures::future::join_all(requests)
            .await
            .into_iter()
            .map(|result| {
                let result = result.unwrap().unwrap();
                serde_json::json!({"auth": {"apiKey": result.api_key}, "source": result.source})
            })
            .collect();
        assert_eq!(
            serde_json::json!(results),
            case["results"],
            "{}",
            case["name"]
        );
        assert_eq!(
            serde_json::to_value(store.read("test").await.unwrap().unwrap()).unwrap(),
            case["credential"],
            "{}",
            case["name"]
        );
        assert_eq!(
            auth.refreshes.load(Ordering::SeqCst) as u64,
            case["refreshes"].as_u64().unwrap()
        );
        assert_eq!(
            store.1.load(Ordering::SeqCst) as u64,
            case["writes"].as_u64().unwrap()
        );
        runtime.shutdown().await;
    }
}

#[tokio::test]
async fn replacement_keeps_only_explicit_attempt_environment_and_provider_resolution() {
    let auth = Arc::new(Auth::new(0));
    let result = AuthRuntime::default()
        .resolve_replacement(
            "test".into(),
            auth.clone(),
            store(None),
            AuthOptions {
                env: [("TRANSPORT_MODE".into(), "test-local".into())].into(),
                ..Default::default()
            },
            CancellationToken::new(),
            Arc::new(FixedClock),
        )
        .await
        .unwrap();
    let AttemptAuth::Replace(result) = result else {
        panic!("expected replacement")
    };
    assert_eq!(
        result.for_provider("test").unwrap().env["TRANSPORT_MODE"],
        "test-local"
    );
    assert_eq!(auth.observed.lock().await.as_ref().unwrap().0.env.len(), 1);
}
