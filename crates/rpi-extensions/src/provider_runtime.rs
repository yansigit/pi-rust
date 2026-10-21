//! Private provider-owned metadata, admission and credential-storage callbacks.
use crate::{ExtensionSession, PluginKeepalive, RegistrySnapshot};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{ffi::c_void, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeManifest {
    pub version: u32,
    pub id: String,
    pub actions: Vec<String>,
}
impl RuntimeManifest {
    pub fn parse(input: &str) -> Result<Self, String> {
        let value: Self =
            serde_json::from_str(input).map_err(|_| "invalid provider runtime manifest")?;
        let mut seen = std::collections::HashSet::new();
        if value.version != 1
            || value.id.trim().is_empty()
            || value.actions.iter().any(|action| {
                !matches!(
                    action.as_str(),
                    "preflight"
                        | "refreshCatalog"
                        | "authMethods"
                        | "credentialStore"
                        | "initialize"
                        | "requestAuth"
                ) || !seen.insert(action)
            })
        {
            return Err("invalid provider runtime manifest".into());
        }
        Ok(value)
    }
}
#[derive(Clone)]
pub struct RegisteredProviderRuntime {
    pub action_bridge: Option<std::sync::Weak<crate::ActionBridge>>,
    pub plugin: String,
    pub manifest: RuntimeManifest,
    pub request_fn: rpi_plugin_sdk::AuthRequestFn,
    pub free: rpi_plugin_sdk::FreeStringFn,
    pub context: *mut c_void,
}
// SAFETY: registration requires callbacks and context callable from any host worker.
unsafe impl Send for RegisteredProviderRuntime {}
unsafe impl Sync for RegisteredProviderRuntime {}

pub struct NativeProviderOperations {
    record: RegisteredProviderRuntime,
    snapshot: Arc<RegistrySnapshot>,
    keepalive: Arc<PluginKeepalive>,
}
impl NativeProviderOperations {
    pub fn from_session(session: &ExtensionSession, id: &str) -> Option<Self> {
        Self::from_parts(session.snapshot_arc()?, session.keepalive(), id)
    }
    pub(crate) fn from_parts(
        snapshot: Arc<RegistrySnapshot>,
        keepalive: Arc<PluginKeepalive>,
        id: &str,
    ) -> Option<Self> {
        let record = snapshot
            .provider_runtimes()
            .iter()
            .find(|record| record.manifest.id == id)?
            .clone();
        let owner = snapshot.provider_owner(id)?;
        if owner.plugin != record.plugin {
            return None;
        }
        Some(Self {
            record,
            snapshot,
            keepalive,
        })
    }
    pub fn supports(&self, action: &str) -> bool {
        self.record
            .manifest
            .actions
            .iter()
            .any(|known| known == action)
    }
    pub async fn request(
        &self,
        action: &str,
        input: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        self.request_inner(action, input, signal, false).await
    }

    /// The host has admitted a credential mutation and holds its storage lock.
    /// Retain this callback and library through completion, including reload.
    pub async fn credential_store(&self, input: Value) -> Result<Value, String> {
        self.request_inner("credentialStore", input, CancellationToken::new(), true)
            .await
    }

    async fn request_inner(
        &self,
        action: &str,
        input: Value,
        signal: CancellationToken,
        committing: bool,
    ) -> Result<Value, String> {
        if (!committing && !self.snapshot.is_active()) || signal.is_cancelled() {
            return Err("provider runtime cancelled".into());
        }
        if !self.supports(action) {
            return Err("provider runtime action is not registered".into());
        }
        // Credential transactions finish independently of a requesting child's lifetime.
        let bridge = if committing {
            None
        } else {
            crate::private_callback::capture_invocation(self.record.action_bridge.as_ref(), &signal)
                .map_err(|_| "provider runtime owner expired")?
        };
        let response = crate::private_callback::invoke_scoped(
            crate::private_callback::Callback {
                request: self.record.request_fn,
                free: self.record.free,
                context: self.record.context,
            },
            self.keepalive.clone(),
            json!({"version":1,"providerId":self.record.manifest.id,"action":action,"input":input})
                .to_string(),
            signal,
            committing,
            bridge,
        )
        .await
        .map_err(|error| match error {
            rpi_ai::auth::AuthError::Cancelled => "provider runtime cancelled".into(),
            _ => "provider runtime callback failed".to_owned(),
        })?;
        if response["version"] != 1 {
            return Err("unsupported provider runtime response".into());
        }
        if let Some(error) = response["error"].as_str() {
            return Err(error.into());
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "provider runtime callback omitted its result".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_manifest_rejects_unsupported_or_ambiguous_actions() {
        for input in [
            json!({"version":2,"id":"provider","actions":[]}),
            json!({"version":1,"id":"provider","actions":["preflight","preflight"]}),
            json!({"version":1,"id":"provider","actions":["fetchCredentials"]}),
            json!({"version":1,"id":"provider","actions":[],"owner":"forged"}),
        ] {
            assert!(RuntimeManifest::parse(&input.to_string()).is_err());
        }
        assert!(RuntimeManifest::parse(&json!({"version":1,"id":"provider","actions":["preflight","refreshCatalog","authMethods","credentialStore","initialize","requestAuth"]}).to_string()).is_ok());
    }
}
