//! Opt-in provider authentication with Pi 1.3.0 refresh semantics.
//!
//! Credentials and resolved request authentication are private execution data.
//! They must never be put in model descriptors or ordinary extension events.
//! Expiry is Unix milliseconds here; legacy stores convert at their boundary.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub const MINIMUM_OAUTH_VALIDITY_MS: i64 = 5 * 60 * 1000;
pub const OAUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthCredential {
    pub access: String,
    #[serde(default)]
    pub refresh: String,
    #[serde(rename = "expires")]
    pub expires_ms: i64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Oauth(OAuthCredential),
}

/// Produced by the registered provider, never by a public model descriptor.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct RequestAuth {
    pub api_key: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub base_url: Option<String>,
    pub env: BTreeMap<String, String>,
    pub source: Option<String>,
}

/// Private execution input, resolved by provider auth and carried across the
/// native request boundary. Model descriptors and event patches cannot supply it.
#[derive(Clone, Default)]
pub enum AttemptAuth {
    #[default]
    Ambient,
    Replace(Arc<ReplacementAuth>),
}

#[derive(Clone)]
pub struct ReplacementAuth {
    provider_id: String,
    auth: RequestAuth,
}

impl ReplacementAuth {
    pub fn for_provider(&self, provider_id: &str) -> Option<&RequestAuth> {
        (self.provider_id == provider_id).then_some(&self.auth)
    }

    /// Decode authentication already resolved by the host across a private
    /// native-provider boundary. Delegation APIs must resolve credentials via
    /// ProviderAuth before creating this value, never accept it from a model.
    pub fn from_private_request(provider_id: String, auth: RequestAuth) -> AttemptAuth {
        AttemptAuth::Replace(Arc::new(Self { provider_id, auth }))
    }
}

impl std::fmt::Debug for AttemptAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Ambient => "Ambient",
            Self::Replace(_) => "Replace([redacted])",
        })
    }
}

// Debug is deliberately unavailable on token-bearing types.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("authentication cancelled")]
    Cancelled,
    #[error("credential store operation failed")]
    Store,
    #[error("provider authentication failed")]
    Resolve,
    #[error("OAuth refresh failed")]
    Refresh,
    #[error("OAuth refresh timed out")]
    RefreshTimeout,
    #[error("OAuth refresh returned a token that expires too soon")]
    InsufficientValidity,
    #[error("OAuth request authentication derivation failed")]
    Derive,
    #[error("authentication task failed")]
    Task,
    /// An explicitly public diagnostic already redacted by the registered provider.
    #[error("{0}")]
    Provider(String),
}

#[derive(Clone, Default)]
pub struct AuthContext {
    /// Request-local snapshot. Account selection must never mutate process env.
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
pub struct AuthOptions {
    pub api_key: Option<String>,
    pub env: BTreeMap<String, String>,
    pub minimum_validity_ms: Option<i64>,
    /// Explicit credential-management refresh, independent of token lifetime.
    pub force_refresh: bool,
}

/// The implementation owns callback contexts and any native-library keepalive.
#[async_trait::async_trait]
pub trait ProviderAuth: Send + Sync {
    /// Capture host-private request scope before credential work moves to a task.
    fn capture_invocation(&self) -> Result<Option<Arc<dyn ProviderAuth>>, AuthError> {
        Ok(None)
    }

    fn supports_api_key(&self) -> bool;
    fn supports_oauth(&self) -> bool;

    async fn resolve_api_key(
        &self,
        context: AuthContext,
        credential: Option<Credential>,
        signal: CancellationToken,
    ) -> Result<Option<RequestAuth>, AuthError>;

    async fn refresh(
        &self,
        credential: OAuthCredential,
        signal: CancellationToken,
    ) -> Result<OAuthCredential, AuthError>;

    async fn to_auth(&self, credential: OAuthCredential) -> Result<RequestAuth, AuthError>;
}

/// A lock held until the replacement is durably stored. Implementations must
/// serialize login/logout against this same transaction, across processes too.
#[async_trait::async_trait]
pub trait CredentialTransaction: Send + Sync {
    fn current(&self) -> Option<Credential>;
    async fn replace(&mut self, credential: Credential) -> Result<(), AuthError>;
    async fn remove(&mut self) -> Result<(), AuthError> {
        Err(AuthError::Store)
    }
}

#[async_trait::async_trait]
pub trait CredentialStore: Send + Sync {
    async fn read(&self, provider: &str) -> Result<Option<Credential>, AuthError>;
    /// Availability probe: does a credential exist, without resolving it?
    ///
    /// Reporting availability must never execute a `!command` API key (or
    /// interpolate a `$VAR` template) just to learn that one is configured. Pi
    /// only resolves at request time (`auth-storage.js:213-214` returns command
    /// values unresolved, `provider-composer.js:266-267` reports a command key
    /// as configured without running it). The default delegates to
    /// [`Self::read`] because plugin-owned stores never execute config values;
    /// the legacy `auth.json` store overrides it with a non-resolving read.
    async fn read_unresolved(&self, provider: &str) -> Result<Option<Credential>, AuthError> {
        self.read(provider).await
    }
    async fn lock(
        &self,
        provider: &str,
        signal: CancellationToken,
    ) -> Result<Box<dyn CredentialTransaction>, AuthError>;
}

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0)
    }
}

#[derive(Default)]
struct RuntimeState {
    stopping: bool,
    active: usize,
}

#[derive(Default)]
struct SharedRuntime {
    state: Mutex<RuntimeState>,
    stopping: CancellationToken,
    completed: tokio::sync::Notify,
}

struct ActiveOperation(Arc<SharedRuntime>);
impl Drop for ActiveOperation {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().active -= 1;
        self.0.completed.notify_waiters();
    }
}

/// Session-owned authentication work. Call `shutdown` before stopping the Tokio
/// runtime or unloading callbacks; cancelled requests may still be persisting.
#[derive(Default)]
pub struct AuthRuntime {
    shared: Arc<SharedRuntime>,
}

impl AuthRuntime {
    /// Resolve an account using its own store and no ambient environment.
    /// An unavailable account fails closed instead of falling back to the
    /// provider's default credential. Store implementations own account locks.
    pub async fn resolve_replacement(
        &self,
        provider_id: String,
        provider: Arc<dyn ProviderAuth>,
        store: Arc<dyn CredentialStore>,
        options: AuthOptions,
        signal: CancellationToken,
        clock: Arc<dyn Clock>,
    ) -> Result<AttemptAuth, AuthError> {
        let mut env = options.env.clone();
        let mut auth = self
            .resolve_scoped(
                provider_id.clone(),
                provider,
                store,
                None,
                options,
                signal,
                clock,
            )
            .await?
            .ok_or(AuthError::Resolve)?;
        env.extend(auth.env);
        auth.env = env;
        Ok(AttemptAuth::Replace(Arc::new(ReplacementAuth {
            provider_id,
            auth,
        })))
    }

    /// Cancellation stops waiting, but cannot discard a rotated refresh token.
    /// The task retains the provider (including its library) through commit.
    pub async fn resolve(
        &self,
        provider_id: String,
        provider: Arc<dyn ProviderAuth>,
        store: Arc<dyn CredentialStore>,
        context: AuthContext,
        options: AuthOptions,
        signal: CancellationToken,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<RequestAuth>, AuthError> {
        self.resolve_scoped(
            provider_id,
            provider,
            store,
            Some(context),
            options,
            signal,
            clock,
        )
        .await
    }

    async fn resolve_scoped(
        &self,
        provider_id: String,
        provider: Arc<dyn ProviderAuth>,
        store: Arc<dyn CredentialStore>,
        context: Option<AuthContext>,
        options: AuthOptions,
        signal: CancellationToken,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<RequestAuth>, AuthError> {
        let provider = provider.capture_invocation()?.unwrap_or(provider);
        self.run_owned(signal, move |work_signal| async move {
            resolve_inner(
                provider_id,
                provider,
                store,
                context,
                options,
                work_signal,
                clock,
            )
            .await
        })
        .await
    }

    /// Own a credential transaction through its durable completion. The signal
    /// cancels admission and lock waits; after beginning a mutation, the work
    /// must finish persistence independently of the requesting caller.
    pub async fn run_owned<T, F, Fut>(
        &self,
        signal: CancellationToken,
        work: F,
    ) -> Result<T, AuthError>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, AuthError>> + Send + 'static,
    {
        let operation = {
            let mut state = self.shared.state.lock().unwrap();
            if state.stopping || signal.is_cancelled() {
                return Err(AuthError::Cancelled);
            }
            state.active += 1;
            ActiveOperation(self.shared.clone())
        };
        let work_signal = self.shared.stopping.child_token();
        let _cancel_on_drop = work_signal.clone().drop_guard();
        let task = tokio::spawn(async move {
            let _operation = operation;
            work(work_signal).await
        });
        tokio::select! {
            biased;
            _ = signal.cancelled() => Err(AuthError::Cancelled),
            _ = self.shared.stopping.cancelled() => Err(AuthError::Cancelled),
            result = task => result.map_err(|_| AuthError::Task)?,
        }
    }

    /// Reject admission and cancel lock waits. Already-started refreshes keep
    /// their independent deadline and finish storing the rotated credential.
    pub fn begin_shutdown(&self) {
        self.shared.state.lock().unwrap().stopping = true;
        self.shared.stopping.cancel();
    }

    pub async fn shutdown(&self) {
        self.begin_shutdown();
        loop {
            let completed = self.shared.completed.notified();
            tokio::pin!(completed);
            completed.as_mut().enable();
            if self.shared.state.lock().unwrap().active == 0 {
                return;
            }
            completed.await;
        }
    }
}

impl Drop for AuthRuntime {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

async fn resolve_inner(
    provider_id: String,
    provider: Arc<dyn ProviderAuth>,
    store: Arc<dyn CredentialStore>,
    context: Option<AuthContext>,
    options: AuthOptions,
    signal: CancellationToken,
    clock: Arc<dyn Clock>,
) -> Result<Option<RequestAuth>, AuthError> {
    let allow_ambient = context.is_some();
    let mut context = context.unwrap_or_default();
    if signal.is_cancelled() {
        return Err(AuthError::Cancelled);
    }
    for (key, value) in &options.env {
        // Pi's overlay env lookup falls back to ambient for an empty override.
        if !value.is_empty() {
            context.env.insert(key.clone(), value.clone());
        }
    }
    if options.api_key.is_some() && provider.supports_api_key() {
        return provider
            .resolve_api_key(
                context,
                Some(Credential::ApiKey {
                    key: options.api_key,
                    env: options.env,
                }),
                signal,
            )
            .await;
    }
    let stored = tokio::select! {
        biased;
        _ = signal.cancelled() => return Err(AuthError::Cancelled),
        stored = store.read(&provider_id) => stored?,
    };
    match stored {
        Some(Credential::Oauth(mut credential)) if provider.supports_oauth() => {
            let minimum = options
                .minimum_validity_ms
                .unwrap_or(0)
                .max(MINIMUM_OAUTH_VALIDITY_MS);
            let needs_refresh = |credential: &OAuthCredential| {
                clock.now_ms().saturating_add(minimum) >= credential.expires_ms
            };
            if options.force_refresh || needs_refresh(&credential) {
                let mut transaction = store.lock(&provider_id, signal.clone()).await?;
                if signal.is_cancelled() {
                    return Err(AuthError::Cancelled);
                }
                let Some(Credential::Oauth(current)) = transaction.current() else {
                    return Ok(None);
                };
                let force = options.force_refresh && current == credential;
                credential = current;
                if force || needs_refresh(&credential) {
                    // Once refresh starts, caller cancellation must not lose
                    // the only valid token. Its deadline is independent.
                    let refresh_signal = CancellationToken::new();
                    let _cancel_refresh = refresh_signal.clone().drop_guard();
                    credential = tokio::time::timeout(
                        OAUTH_REFRESH_TIMEOUT,
                        provider.refresh(credential, refresh_signal),
                    )
                    .await
                    .map_err(|_| AuthError::RefreshTimeout)??;
                    transaction
                        .replace(Credential::Oauth(credential.clone()))
                        .await?;
                }
                drop(transaction);
                if options.minimum_validity_ms.is_some() && needs_refresh(&credential) {
                    return Err(AuthError::InsufficientValidity);
                }
            }
            let mut auth = tokio::select! {
                biased;
                _ = signal.cancelled() => return Err(AuthError::Cancelled),
                result = provider.to_auth(credential) => result?,
            };
            auth.source = Some("OAuth".into());
            Ok(Some(auth))
        }
        Some(Credential::ApiKey { key, mut env }) if provider.supports_api_key() => {
            env.extend(options.env);
            provider
                .resolve_api_key(context, Some(Credential::ApiKey { key, env }), signal)
                .await
        }
        Some(_) => Ok(None),
        None if allow_ambient && provider.supports_api_key() => {
            provider.resolve_api_key(context, None, signal).await
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests;
