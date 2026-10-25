//! Private provider authentication. Callback output never enters event dispatch.

use std::{ffi::c_void, sync::Arc};

use rpi_ai::auth::{
    AuthContext, AuthError, Credential, OAuthCredential, ProviderAuth, RequestAuth,
};
#[cfg(test)]
use rpi_plugin_sdk::{AuthRequestControl, StbString, StbStringRef};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{ExtensionSession, PluginKeepalive, RegistrySnapshot};

mod compatibility;
mod interaction;
mod legacy;
pub use compatibility::ProviderAuthentication;
pub use interaction::{AuthChoice, AuthInteraction};
pub use legacy::LegacyOAuthAuth;

/// Public method discovery.
///
/// `login_label` carries Pi's `oauth.loginLabel`, which the multiprovider rows
/// render as `loginLabel ?? name`.
/// `interactive_login` carries whether Pi's `apiKey.login` exists: `Some(false)`
/// means discovery only, and a host must fall back to a paste prompt. Both are
/// optional so payloads written before they existed still parse, and unchanged
/// methods keep their previous JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    pub kind: AuthMethodKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive_login: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethodKind {
    ApiKey,
    Oauth,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthManifest {
    pub version: u32,
    pub id: String,
    pub display_name: String,
    pub api_key: bool,
    pub oauth: bool,
    pub methods: Vec<AuthMethod>,
}

impl AuthManifest {
    pub fn parse(value: &str) -> Result<Self, AuthError> {
        let manifest: Self = serde_json::from_str(value).map_err(|_| AuthError::Resolve)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), AuthError> {
        let mut ids = std::collections::BTreeSet::new();
        if self.version != 1
            || self.id.trim().is_empty()
            || self.display_name.trim().is_empty()
            || self.methods.iter().any(|method| {
                method.id.trim().is_empty()
                    || method.name.trim().is_empty()
                    || method
                        .login_label
                        .as_deref()
                        .is_some_and(|label| label.trim().is_empty())
                    || !ids.insert(&method.id)
                    || match method.kind {
                        AuthMethodKind::ApiKey => !self.api_key,
                        AuthMethodKind::Oauth => !self.oauth,
                    }
            })
        {
            return Err(AuthError::Resolve);
        }
        Ok(())
    }
}

/// The plugin owns its context until shutdown has drained outstanding calls.
#[derive(Clone)]
pub struct RegisteredAuthProvider {
    pub action_bridge: Option<std::sync::Weak<crate::ActionBridge>>,
    pub plugin: String,
    pub manifest: AuthManifest,
    pub request_fn: rpi_plugin_sdk::AuthRequestFn,
    pub plugin_free_string: rpi_plugin_sdk::FreeStringFn,
    pub user_data: *mut c_void,
}

// SAFETY: registration requires a context callable from any host worker thread.
unsafe impl Send for RegisteredAuthProvider {}
unsafe impl Sync for RegisteredAuthProvider {}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthAction {
    Login,
    Continue,
    Refresh,
    ResolveApiKey,
    ToAuth,
    Revoke,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthResponseKind {
    Interaction,
    Credential,
    Auth,
    Success,
    Unavailable,
    Error,
}

// No Debug: login continuations and payloads can contain credentials.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthResponse {
    pub version: u32,
    pub kind: AuthResponseKind,
    pub payload: Value,
}

pub struct NativeProviderAuth {
    invocation: Option<Arc<crate::ActionBridge>>,
    _background: Option<rpi_agent::tool_execution_scope::BackgroundToolGuard>,
    record: RegisteredAuthProvider,
    snapshot: Arc<RegistrySnapshot>,
    keepalive: Arc<PluginKeepalive>,
}

impl NativeProviderAuth {
    pub fn from_session(session: &ExtensionSession, id: &str) -> Option<Arc<Self>> {
        let snapshot = session.snapshot_arc()?;
        let record = snapshot
            .auth_providers()
            .iter()
            .find(|p| p.manifest.id == id)?
            .clone();
        Some(Arc::new(Self {
            invocation: None,
            _background: None,
            record,
            snapshot,
            keepalive: session.keepalive(),
        }))
    }

    pub fn manifest(&self) -> &AuthManifest {
        &self.record.manifest
    }

    /// Discovery and login consult the same current methods, including custody refusals.
    pub async fn methods(&self, signal: CancellationToken) -> Result<Vec<AuthMethod>, AuthError> {
        if !self.snapshot.is_active() || signal.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let mut manifest = self.manifest().clone();
        if let Some(operations) = crate::provider_runtime::NativeProviderOperations::from_parts(
            self.snapshot.clone(),
            self.keepalive.clone(),
            &manifest.id,
        )
        .filter(|operations| operations.supports("authMethods"))
        {
            let value = operations
                .request("authMethods", json!({}), signal.clone())
                .await
                .map_err(|error| {
                    if signal.is_cancelled() {
                        AuthError::Cancelled
                    } else {
                        AuthError::Provider(error)
                    }
                })?;
            manifest.methods = serde_json::from_value(value).map_err(|_| AuthError::Resolve)?;
            manifest.validate()?;
        }
        Ok(manifest.methods)
    }

    /// Drive the provider's flow through an awaited host presentation port.
    /// Only an acknowledged store commit completes login successfully.
    pub async fn login<F, Fut>(
        &self,
        method_id: &str,
        store: Arc<dyn rpi_ai::auth::CredentialStore>,
        signal: CancellationToken,
        mut interact: F,
    ) -> Result<(), AuthError>
    where
        F: FnMut(AuthInteraction) -> Fut,
        Fut: std::future::Future<Output = Result<Value, AuthError>>,
    {
        if !self
            .methods(signal.clone())
            .await?
            .iter()
            .any(|method| method.id == method_id)
        {
            return Err(AuthError::Resolve);
        }
        let mut action = AuthAction::Login;
        let mut input = json!({"methodId":method_id});
        loop {
            let response = self.request(action, input, signal.clone()).await?;
            match response.kind {
                AuthResponseKind::Credential => {
                    let credential: Credential =
                        serde_json::from_value(response.payload).map_err(|_| AuthError::Resolve)?;
                    if !matches!(&credential,
                        Credential::Oauth(_) if self.manifest().oauth)
                        && !matches!(&credential, Credential::ApiKey { .. } if self.manifest().api_key)
                    {
                        return Err(AuthError::Resolve);
                    }
                    let mut transaction = store.lock(&self.manifest().id, signal).await?;
                    return transaction.replace(credential).await;
                }
                AuthResponseKind::Interaction => {
                    let interaction = AuthInteraction::parse(response.payload)?;
                    input = tokio::select! {
                        biased;
                        _ = signal.cancelled() => return Err(AuthError::Cancelled),
                        result = interact(interaction) => result?,
                    };
                    action = AuthAction::Continue;
                }
                _ => return Err(AuthError::Resolve),
            }
        }
    }

    pub async fn request(
        &self,
        action: AuthAction,
        input: Value,
        signal: CancellationToken,
    ) -> Result<AuthResponse, AuthError> {
        if !self.snapshot.is_active() || signal.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let request =
            json!({"version":1,"providerId":self.record.manifest.id,"action":action,"input":input})
                .to_string();
        let bridge = match &self.invocation {
            Some(bridge) => Some(bridge.clone()),
            None => crate::private_callback::capture_invocation(
                self.record.action_bridge.as_ref(),
                &signal,
            )?,
        };
        let value = crate::private_callback::invoke_scoped(
            crate::private_callback::Callback {
                request: self.record.request_fn,
                free: self.record.plugin_free_string,
                context: self.record.user_data,
            },
            self.keepalive.clone(),
            request,
            signal,
            false,
            bridge,
        )
        .await?;
        let response: AuthResponse =
            serde_json::from_value(value).map_err(|_| AuthError::Resolve)?;
        if response.version != 1 {
            return Err(AuthError::Resolve);
        }
        if response.kind == AuthResponseKind::Error {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct PublicError {
                message: String,
            }
            let error: PublicError =
                serde_json::from_value(response.payload).map_err(|_| AuthError::Resolve)?;
            return Err(AuthError::Provider(error.message));
        }
        Ok(response)
    }
}

#[async_trait::async_trait]
impl ProviderAuth for NativeProviderAuth {
    fn capture_invocation(&self) -> Result<Option<Arc<dyn ProviderAuth>>, AuthError> {
        let invocation = crate::private_callback::capture_invocation(
            self.record.action_bridge.as_ref(),
            &CancellationToken::new(),
        )?;
        let background = invocation
            .as_ref()
            .and_then(|bridge| bridge.tool_scope.as_ref())
            .map(|scope| scope.track_background());
        Ok(Some(Arc::new(Self {
            invocation,
            _background: background,
            record: self.record.clone(),
            snapshot: self.snapshot.clone(),
            keepalive: self.keepalive.clone(),
        })))
    }
    fn supports_api_key(&self) -> bool {
        self.record.manifest.api_key
    }
    fn supports_oauth(&self) -> bool {
        self.record.manifest.oauth
    }

    async fn resolve_api_key(
        &self,
        context: AuthContext,
        credential: Option<Credential>,
        signal: CancellationToken,
    ) -> Result<Option<RequestAuth>, AuthError> {
        let response = self
            .request(
                AuthAction::ResolveApiKey,
                json!({"env":context.env,"credential":credential}),
                signal,
            )
            .await?;
        match response.kind {
            AuthResponseKind::Auth => serde_json::from_value(response.payload)
                .map(Some)
                .map_err(|_| AuthError::Resolve),
            AuthResponseKind::Unavailable => Ok(None),
            _ => Err(AuthError::Resolve),
        }
    }

    async fn refresh(
        &self,
        credential: OAuthCredential,
        signal: CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let response = self
            .request(
                AuthAction::Refresh,
                json!({"credential":credential}),
                signal,
            )
            .await
            .map_err(|error| match error {
                AuthError::Provider(_) => error,
                _ => AuthError::Refresh,
            })?;
        if response.kind != AuthResponseKind::Credential {
            return Err(AuthError::Refresh);
        }
        serde_json::from_value(response.payload).map_err(|_| AuthError::Refresh)
    }

    async fn to_auth(&self, credential: OAuthCredential) -> Result<RequestAuth, AuthError> {
        let response = self
            .request(
                AuthAction::ToAuth,
                json!({"credential":credential}),
                CancellationToken::new(),
            )
            .await
            .map_err(|error| match error {
                AuthError::Provider(_) => error,
                _ => AuthError::Derive,
            })?;
        if response.kind != AuthResponseKind::Auth {
            return Err(AuthError::Derive);
        }
        serde_json::from_value(response.payload).map_err(|_| AuthError::Derive)
    }
}

#[cfg(test)]
mod tests;
