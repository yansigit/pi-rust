//! Session-owned catalog publication. Readers capture an immutable execution
//! chain; publication rebuilds wrappers from bases and acknowledges one revision.
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use rpi_ai::{AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions};
use rpi_extensions::{
    native::{NativeOwner, NativeProviderRuntime},
    ExtensionSession, RuntimeActionHost,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::model_registry::ModelRegistry;
mod auth;

struct State {
    catalog: ModelRegistry,
    chains: BTreeMap<String, Arc<dyn Provider>>,
}
pub(crate) struct ProviderRuntime {
    session: ExtensionSession,
    generation: String,
    host: Arc<dyn RuntimeActionHost>,
    myself: Weak<Self>,
    state: Mutex<State>,
    logins: Mutex<BTreeMap<String, auth::Login>>,
}
impl ProviderRuntime {
    pub(crate) fn new(
        session: &ExtensionSession,
        bridge: &rpi_extensions::ActionBridge,
        providers: Vec<Arc<dyn Provider>>,
        discovery: Vec<Model>,
    ) -> Result<Arc<Self>, String> {
        let catalog = ModelRegistry::new();
        for provider in providers {
            let owner = session
                .snapshot()
                .and_then(|s| s.provider_owner(provider.id()))
                .cloned();
            catalog
                .register_runtime(owner, provider)
                .map_err(|e| e.to_string())?;
        }
        let mut discovered: BTreeMap<String, Vec<Model>> = BTreeMap::new();
        for model in discovery {
            if !catalog.has_provider(&model.provider) {
                discovered
                    .entry(model.provider.clone())
                    .or_default()
                    .push(model);
            }
        }
        for (id, models) in discovered {
            let owner = session
                .snapshot()
                .and_then(|s| s.provider_owner(&id))
                .cloned();
            catalog
                .register_discovered(owner, id, models)
                .map_err(|e| e.to_string())?;
        }
        if let Some(snapshot) = session.snapshot() {
            for manifest in crate::provider::manifests_from_session(session) {
                if !catalog.has_provider(&manifest.id) {
                    catalog
                        .register_discovered(
                            snapshot.provider_owner(&manifest.id).cloned(),
                            manifest.id.clone(),
                            manifest.models.clone(),
                        )
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        let chains = Self::chains(session, &catalog);
        let runtime = Arc::new_cyclic(|myself| Self {
            session: session.clone(),
            generation: bridge.generation().into(),
            host: bridge.clone_host(),
            myself: myself.clone(),
            state: Mutex::new(State { catalog, chains }),
            logins: Mutex::new(BTreeMap::new()),
        });
        Ok(runtime)
    }
    pub(crate) fn activate(self: &Arc<Self>) {
        let port: Arc<dyn NativeProviderRuntime> = self.clone();
        self.host
            .bind_provider_runtime_generation(&self.generation, Arc::downgrade(&port));
    }
    pub(crate) async fn initialize(
        session: &ExtensionSession,
        bridge: &rpi_extensions::ActionBridge,
    ) -> Result<Arc<Self>, String> {
        // The credential port must exist before model resolution reads auth.json.
        let runtime = Self::new(session, bridge, Vec::new(), Vec::new())?;
        runtime.activate();
        if let Some(snapshot) = session.snapshot() {
            for record in snapshot.provider_runtimes() {
                if !record
                    .manifest
                    .actions
                    .iter()
                    .any(|action| action == "initialize")
                {
                    continue;
                }
                let Some(operations) =
                    rpi_extensions::provider_runtime::NativeProviderOperations::from_session(
                        session,
                        &record.manifest.id,
                    )
                else {
                    session.auth_runtime().shutdown().await;
                    return Err("provider initialization has no owning registration".into());
                };
                let agent_dir = crate::config::agent_dir()
                    .map_err(|_| "provider storage directory is unavailable")?;
                if let Err(error) = operations
                    .request(
                        "initialize",
                        json!({"agentDir":agent_dir}),
                        CancellationToken::new(),
                    )
                    .await
                {
                    session.auth_runtime().shutdown().await;
                    return Err(error);
                }
            }
        }
        Ok(runtime)
    }
    fn chains(
        session: &ExtensionSession,
        catalog: &ModelRegistry,
    ) -> BTreeMap<String, Arc<dyn Provider>> {
        rpi_extensions::wrap_providers(session, catalog.instances())
            .into_iter()
            .map(|provider| (provider.id().into(), provider))
            .collect()
    }
    fn proxies(&self, catalog: &ModelRegistry) -> Vec<Arc<dyn Provider>> {
        let runtime = self
            .myself
            .upgrade()
            .expect("runtime call retains its owner");
        catalog
            .instances()
            .into_iter()
            .map(|provider| {
                Arc::new(RuntimeProvider {
                    id: provider.id().into(),
                    models: provider.models().to_vec(),
                    runtime: runtime.clone(),
                }) as Arc<dyn Provider>
            })
            .collect()
    }
    pub(crate) async fn providers(&self) -> Vec<Arc<dyn Provider>> {
        self.proxies(&self.state.lock().await.catalog)
    }
    fn validate_owner(&self, owner: &NativeOwner) -> Result<(), String> {
        if owner.generation != self.generation
            || self.session.snapshot().is_some_and(|s| !s.is_active())
        {
            return Err("inactive provider runtime generation".into());
        }
        Ok(())
    }
    async fn catalog_request(
        &self,
        owner: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let mut state = self.state.lock().await;
        self.validate_owner(&owner)?;
        if signal.is_cancelled() {
            return Err("provider runtime request cancelled".into());
        }
        let action = args["action"].as_str().ok_or("missing provider action")?;
        if action == "inspect" {
            return Ok(
                json!({"revision":state.catalog.revision(),"providers":state.catalog.list_providers()}),
            );
        }
        let id = args["providerId"]
            .as_str()
            .ok_or("missing provider identity")?;
        let source = if action == "register" {
            args["implementationId"]
                .as_str()
                .ok_or("missing provider implementation")?
        } else {
            id
        };
        let registered_owner = if action == "register" {
            self.session
                .snapshot()
                .and_then(|s| s.provider_owner(source))
                .cloned()
        } else {
            state.catalog.owner(id)
        };
        if owner.child || registered_owner.as_ref() != Some(&owner) {
            return Err("provider belongs to a different session or plugin".into());
        }
        if args
            .get("expectedRevision")
            .is_some_and(|revision| revision.as_u64() != Some(state.catalog.revision()))
        {
            return Err("provider catalog changed during the operation".into());
        }
        let candidate = state.catalog.fork().map_err(|e| e.to_string())?;
        match action {
            "publish" | "register" => {
                let models: Vec<Model> = serde_json::from_value(args["models"].clone())
                    .map_err(|_| "invalid provider catalog")?;
                if models
                    .iter()
                    .any(|model| model.provider != id || model.headers.is_some())
                {
                    return Err(
                        "public catalogs cannot carry credentials or change provider identity"
                            .into(),
                    );
                }
                let provider = if action == "register" {
                    crate::provider::native_alias(&self.session, source, id, models)
                        .ok_or("invalid native provider implementation or catalog")?
                } else {
                    candidate
                        .get_provider_instance(id)
                        .ok_or("provider is not registered")?
                        .with_catalog(models)
                        .ok_or("provider cannot replace its execution catalog")?
                };
                candidate
                    .publish_owned(owner, provider)
                    .map_err(|e| e.to_string())?;
            }
            "unregister" => {
                candidate
                    .unregister_owned(&owner, id)
                    .map_err(|e| e.to_string())?;
            }
            _ => return Err("unsupported provider runtime action".into()),
        }
        let chains = Self::chains(&self.session, &candidate);
        // The same gate protects readers, so the installed proxies cannot see
        // a catalog reservation before the host has accepted the update.
        self.host
            .install_native_providers(self.proxies(&candidate))
            .await?;
        self.host.replace_model_catalog(
            candidate
                .instances()
                .iter()
                .flat_map(|provider| provider.models().to_vec())
                .collect(),
        );
        let revision = candidate.revision();
        *state = State {
            catalog: candidate,
            chains,
        };
        Ok(json!({"revision":revision}))
    }
    async fn refresh(
        &self,
        caller: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let id = args["providerId"]
            .as_str()
            .ok_or("missing provider identity")?;
        let (owner, revision) = {
            let state = self.state.lock().await;
            (
                state
                    .catalog
                    .owner(id)
                    .ok_or("provider has no native catalog owner")?,
                state.catalog.revision(),
            )
        };
        self.validate_owner(&caller)?;
        let operations = rpi_extensions::provider_runtime::NativeProviderOperations::from_session(
            &self.session,
            id,
        )
        .ok_or("provider has no catalog refresh callback")?;
        let result = operations.request("refreshCatalog", json!({"allowNetwork":args["allowNetwork"].as_bool().unwrap_or(false),"force":args["force"].as_bool().unwrap_or(false)}), signal.clone()).await?;
        self.catalog_request(owner, json!({"action":"publish","providerId":id,"models":result["models"],"expectedRevision":revision}), signal).await
    }
}
#[async_trait::async_trait]
impl NativeProviderRuntime for ProviderRuntime {
    async fn request(
        &self,
        owner: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        self.validate_owner(&owner)?;
        match args["port"].as_str() {
            Some("providers") if args["action"] == "refresh" => {
                self.refresh(owner, args, signal).await
            }
            Some("providers") => self.catalog_request(owner, args, signal).await,
            Some("auth") => self.auth_request(owner, args, signal).await,
            _ => Err("unsupported native runtime port".into()),
        }
    }
}
struct RuntimeProvider {
    id: String,
    models: Vec<Model>,
    runtime: Arc<ProviderRuntime>,
}
#[async_trait::async_trait]
impl Provider for RuntimeProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let provider = self
            .runtime
            .state
            .lock()
            .await
            .chains
            .get(&self.id)
            .cloned();
        match provider {
            Some(provider) => match provider.preflight(model, options.signal.clone()).await {
                Ok(()) => {
                    rpi_ai::provider::forward_stream(provider.as_ref(), model, context, options)
                        .await
                }
                Err(error) => error_stream(model, error).await,
            },
            None => error_stream(model, "provider is no longer registered".into()).await,
        }
    }
}

pub(crate) async fn error_stream(model: &Model, message: String) -> AssistantMessageEventStream {
    let (mut producer, stream) = rpi_ai::event_stream::create_assistant_message_event_stream();
    producer
        .push(rpi_ai::AssistantMessageEvent::Error {
            reason: rpi_ai::types::ErrorReason::Error,
            error: rpi_ai::AssistantMessage::terminal(
                model.api.clone(),
                &model.provider,
                &model.id,
                rpi_ai::types::StopReason::Error,
                message,
                0,
            ),
        })
        .await;
    stream
}

#[cfg(test)]
mod tests;
