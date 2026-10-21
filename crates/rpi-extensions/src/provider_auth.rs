//! Private provider authentication. Callback output never enters event dispatch.

use std::{ffi::c_void, sync::Arc};

use rpi_ai::auth::{
    AuthContext, AuthError, Credential, OAuthCredential, ProviderAuth, RequestAuth,
};
use rpi_plugin_sdk::{AuthRequestControl, StbString, StbStringRef};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{ExtensionSession, PluginKeepalive, RegistrySnapshot};

mod interaction;
mod legacy;
pub use interaction::{AuthChoice, AuthInteraction};
pub use legacy::LegacyOAuthAuth;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    pub kind: AuthMethodKind,
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
        let mut ids = std::collections::BTreeSet::new();
        if manifest.version != 1
            || manifest.id.trim().is_empty()
            || manifest.display_name.trim().is_empty()
            || manifest.methods.iter().any(|method| {
                method.id.trim().is_empty()
                    || method.name.trim().is_empty()
                    || !ids.insert(&method.id)
                    || match method.kind {
                        AuthMethodKind::ApiKey => !manifest.api_key,
                        AuthMethodKind::Oauth => !manifest.oauth,
                    }
            })
        {
            return Err(AuthError::Resolve);
        }
        Ok(manifest)
    }
}

/// The plugin owns its context until shutdown has drained outstanding calls.
#[derive(Clone)]
pub struct RegisteredAuthProvider {
    pub plugin: String,
    pub manifest: AuthManifest,
    pub request_fn: rpi_plugin_sdk::AuthRequestFn,
    pub plugin_free_string: rpi_plugin_sdk::FreeStringFn,
    pub user_data: *mut c_void,
}

// SAFETY: registration requires a context callable from any host worker thread.
unsafe impl Send for RegisteredAuthProvider {}
unsafe impl Sync for RegisteredAuthProvider {}

impl RegisteredAuthProvider {
    fn invoke(&self, request: &str, control: &AuthRequestControl, out: &mut StbString) -> i32 {
        (self.request_fn)(
            StbStringRef::from_str(request),
            control,
            out,
            self.user_data,
        )
    }
}

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

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthResponseKind {
    Interaction,
    Credential,
    Auth,
    Success,
    Unavailable,
}

// No Debug: login continuations and payloads can contain credentials.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthResponse {
    pub version: u32,
    pub kind: AuthResponseKind,
    pub payload: Value,
}

pub struct NativeProviderAuth {
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
            record,
            snapshot,
            keepalive: session.keepalive(),
        }))
    }

    pub fn manifest(&self) -> &AuthManifest {
        &self.record.manifest
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
            .manifest()
            .methods
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
        let record = self.record.clone();
        let keepalive = self.keepalive.clone();
        let signal = signal.child_token();
        let _cancel_callback = signal.clone().drop_guard();
        let callback_signal = signal.clone();
        let pending = tokio::task::spawn_blocking(move || {
            // Pin both the library and its callback context through output reclamation,
            // even when the caller stops waiting or its session is invalidated.
            let _keepalive = keepalive;
            let control = AuthRequestControl {
                struct_size: std::mem::size_of::<AuthRequestControl>() as u32,
                is_cancelled,
                user_data: (&callback_signal as *const CancellationToken)
                    .cast_mut()
                    .cast(),
            };
            let mut out = StbString::empty();
            let status = record.invoke(&request, &control, &mut out);
            // SAFETY: the callback transfers its output to its own deallocator.
            let out = unsafe {
                rpi_plugin_sdk::callbacks::OwnedString::from_raw(
                    out,
                    Some(record.plugin_free_string),
                )
            };
            if status != 0 {
                return Err(AuthError::Resolve);
            }
            let response: AuthResponse =
                serde_json::from_str(&out.to_string_lossy()).map_err(|_| AuthError::Resolve)?;
            if response.version != 1 {
                return Err(AuthError::Resolve);
            }
            Ok(response)
        });
        tokio::select! {
            biased;
            _ = signal.cancelled() => Err(AuthError::Cancelled),
            result = pending => result.map_err(|_| AuthError::Task)?,
        }
    }
}

extern "C" fn is_cancelled(user_data: *mut c_void) -> u8 {
    // SAFETY: the callback borrows the stack-owned token until it returns.
    u8::from(unsafe { &*user_data.cast::<CancellationToken>() }.is_cancelled())
}

#[async_trait::async_trait]
impl ProviderAuth for NativeProviderAuth {
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
            .map_err(|_| AuthError::Refresh)?;
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
            .map_err(|_| AuthError::Derive)?;
        if response.kind != AuthResponseKind::Auth {
            return Err(AuthError::Derive);
        }
        serde_json::from_value(response.payload).map_err(|_| AuthError::Derive)
    }
}

#[cfg(test)]
mod tests;
