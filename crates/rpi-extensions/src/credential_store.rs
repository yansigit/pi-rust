//! Account-store port for delegated authentication. Credentials stay private.
use rpi_ai::auth::{AuthError, Credential, CredentialStore, CredentialTransaction};
use rpi_plugin_sdk::{AuthRequestFn, CredentialStoreReleaseFn, FreeStringFn, StbStringRef};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{ffi::c_void, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreManifest {
    pub version: u32,
    pub id: String,
}
impl StoreManifest {
    pub fn parse(value: &str) -> Result<Self, AuthError> {
        let manifest: Self = serde_json::from_str(value).map_err(|_| AuthError::Store)?;
        if manifest.version != 1 || manifest.id.trim().is_empty() {
            return Err(AuthError::Store);
        }
        Ok(manifest)
    }
}

#[derive(Clone)]
pub struct RegisteredCredentialStore {
    pub plugin: String,
    pub manifest: StoreManifest,
    pub request: AuthRequestFn,
    pub release: CredentialStoreReleaseFn,
    pub free: FreeStringFn,
    pub context: *mut c_void,
}
// SAFETY: registration requires thread-safe callbacks and a library-owned context.
unsafe impl Send for RegisteredCredentialStore {}
unsafe impl Sync for RegisteredCredentialStore {}

/// A single account in a registered store. The callback must validate both
/// provider and account identity, and serialize the transaction across processes.
pub struct NativeCredentialStore {
    record: RegisteredCredentialStore,
    snapshot: Arc<crate::RegistrySnapshot>,
    keepalive: Arc<crate::PluginKeepalive>,
    provider: String,
    account: String,
}

impl NativeCredentialStore {
    pub fn from_session(
        session: &crate::ExtensionSession,
        store_id: &str,
        provider: &str,
        account: &str,
    ) -> Option<Arc<Self>> {
        if provider.is_empty() || account.is_empty() {
            return None;
        }
        let snapshot = session.snapshot_arc()?;
        let record = snapshot
            .credential_stores()
            .iter()
            .find(|record| record.manifest.id == store_id)?
            .clone();
        Some(Arc::new(Self {
            record,
            snapshot,
            keepalive: session.keepalive(),
            provider: provider.into(),
            account: account.into(),
        }))
    }

    fn check(&self, provider: &str, signal: &CancellationToken) -> Result<(), AuthError> {
        if provider != self.provider {
            return Err(AuthError::Store);
        }
        if !self.snapshot.is_active() || signal.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        Ok(())
    }

    async fn request(
        &self,
        action: &str,
        input: Value,
        signal: CancellationToken,
    ) -> Result<Value, AuthError> {
        crate::private_callback::invoke(
            crate::private_callback::Callback {
                request: self.record.request,
                free: self.record.free,
                context: self.record.context,
            },
            self.keepalive.clone(),
            json!({"version":1,"storeId":self.record.manifest.id,
            "providerId":self.provider,"accountId":self.account,"action":action,"input":input})
            .to_string(),
            signal,
            true,
        )
        .await
        .map_err(|error| {
            if error == AuthError::Cancelled {
                error
            } else {
                AuthError::Store
            }
        })
    }
}

struct Transaction {
    store: Arc<NativeCredentialStore>,
    lock_id: String,
    current: Option<Credential>,
}
impl Drop for Transaction {
    fn drop(&mut self) {
        // A leaf release callback never performs network or persistence work.
        // Keeping the store here pins its library until the lock is released.
        (self.store.record.release)(
            StbStringRef::from_str(&self.lock_id),
            self.store.record.context,
        );
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u32,
    kind: String,
    payload: Value,
}
fn response(value: Value, kind: &str) -> Result<Value, AuthError> {
    let response: Response = serde_json::from_value(value).map_err(|_| AuthError::Store)?;
    if response.version != 1 || response.kind != kind {
        return Err(AuthError::Store);
    }
    Ok(response.payload)
}

#[async_trait::async_trait]
impl CredentialTransaction for Transaction {
    fn current(&self) -> Option<Credential> {
        self.current.clone()
    }
    async fn replace(&mut self, credential: Credential) -> Result<(), AuthError> {
        let result = self
            .store
            .request(
                "replace",
                json!({"lockId":self.lock_id,"credential":credential}),
                CancellationToken::new(),
            )
            .await?;
        response(result, "success")?;
        self.current = Some(credential);
        Ok(())
    }
    async fn remove(&mut self) -> Result<(), AuthError> {
        let result = self
            .store
            .request(
                "remove",
                json!({"lockId":self.lock_id}),
                CancellationToken::new(),
            )
            .await?;
        response(result, "success")?;
        self.current = None;
        Ok(())
    }
}

/// The handle owns an Arc so a cancelled lock waiter can finish acquiring and
/// immediately release a late transaction without leaking foreign store locks.
#[derive(Clone)]
pub struct NativeStoreHandle(pub Arc<NativeCredentialStore>);

#[async_trait::async_trait]
impl CredentialStore for NativeStoreHandle {
    async fn read(&self, provider: &str) -> Result<Option<Credential>, AuthError> {
        self.0.check(provider, &CancellationToken::new())?;
        let result = self
            .0
            .request("read", Value::Null, CancellationToken::new())
            .await?;
        serde_json::from_value(response(result, "credential")?).map_err(|_| AuthError::Store)
    }
    async fn lock(
        &self,
        provider: &str,
        signal: CancellationToken,
    ) -> Result<Box<dyn CredentialTransaction>, AuthError> {
        self.0.check(provider, &signal)?;
        let store = self.0.clone();
        let signal = signal.child_token();
        let _cancel_wait = signal.clone().drop_guard();
        let request_signal = signal.clone();
        let pending = tokio::spawn(async move {
            let result = store.request("lock", Value::Null, request_signal).await?;
            // Construct the guard before decoding credentials: malformed or
            // cancelled responses must release a successfully acquired lock.
            let lock_id = result["payload"]["lockId"]
                .as_str()
                .ok_or(AuthError::Store)?
                .to_owned();
            let mut transaction = Transaction {
                store,
                lock_id,
                current: None,
            };
            if transaction.lock_id.is_empty() || transaction.lock_id.len() > 4096 {
                return Err(AuthError::Store);
            }
            let payload = response(result, "locked")?;
            transaction.current =
                serde_json::from_value(payload.get("credential").ok_or(AuthError::Store)?.clone())
                    .map_err(|_| AuthError::Store)?;
            Ok(Box::new(transaction) as Box<dyn CredentialTransaction>)
        });
        tokio::select! {
            biased;
            _ = signal.cancelled() => Err(AuthError::Cancelled),
            result = pending => result.map_err(|_| AuthError::Task)?,
        }
    }
}

#[cfg(test)]
mod tests;
