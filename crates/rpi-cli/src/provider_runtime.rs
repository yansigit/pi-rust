//! Session-owned catalog publication. Readers capture an immutable execution
//! chain; publication rebuilds wrappers from bases and acknowledges one revision.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use rpi_ai::{AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions};
use rpi_extensions::delegation::{
    DelegationBinding, DelegationCaller, DelegationEpoch, DelegationResolver, DelegationScopeId,
    ResolvedAmbientAuth, ResolvedDelegationTarget,
};
use rpi_extensions::{
    native::{NativeOwner, NativeProviderRuntime},
    ExtensionSession, RuntimeActionHost,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::model_registry::ModelRegistry;
mod auth;
mod delegation;

struct State {
    epoch: Arc<DelegationEpoch>,
    catalog: ModelRegistry,
    config: crate::config::ModelsConfig,
    chains: BTreeMap<String, Arc<dyn Provider>>,
    generations: BTreeMap<String, u64>,
    refreshes: BTreeMap<String, Weak<Publication>>,
}
struct Publication {
    owner: NativeOwner,
    provider: String,
    generation: u64,
    signal: CancellationToken,
    committed: AtomicU64,
}
impl Drop for Publication {
    fn drop(&mut self) {
        self.signal.cancel();
    }
}
impl State {
    fn generation(&self, id: &str) -> u64 {
        self.generations.get(id).copied().unwrap_or(0)
    }
    fn next_generation(&self, id: &str) -> Result<u64, String> {
        self.generation(id)
            .checked_add(1)
            .ok_or_else(|| "provider refresh generation exhausted".into())
    }
    fn supersede(&mut self, id: &str, generation: u64) {
        self.generations.insert(id.into(), generation);
        if let Some(previous) = self
            .refreshes
            .remove(id)
            .and_then(|refresh| refresh.upgrade())
        {
            previous.signal.cancel();
        }
    }
}
pub(crate) struct ProviderRuntime {
    delegation_scope: DelegationScopeId,
    session: ExtensionSession,
    generation: String,
    host: Arc<dyn RuntimeActionHost>,
    myself: Weak<Self>,
    state: Mutex<State>,
    scope: Option<Weak<rpi_agent::tool_execution_scope::ToolExecutionScope>>,
    children: Mutex<BTreeMap<String, Arc<Self>>>,
    logins: Mutex<BTreeMap<String, auth::Login>>,
    auth: BTreeMap<String, Arc<crate::provider::auth::AuthBinding>>,
    publications: std::sync::Mutex<BTreeMap<String, Weak<Publication>>>,
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
        let auth = crate::provider::native_auth_bindings(session)?;
        let delegation_scope = DelegationScopeId::fresh();
        let epoch = DelegationEpoch::new(delegation_scope, chains.values().cloned());
        let runtime = Arc::new_cyclic(|myself| Self {
            delegation_scope,
            session: session.clone(),
            generation: bridge.generation().into(),
            host: bridge.clone_host(),
            myself: myself.clone(),
            scope: None,
            children: Mutex::new(BTreeMap::new()),
            state: Mutex::new(State {
                epoch,
                catalog,
                config: Default::default(),
                chains,
                generations: BTreeMap::new(),
                refreshes: BTreeMap::new(),
            }),
            logins: Mutex::new(BTreeMap::new()),
            auth,
            publications: Default::default(),
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
    async fn for_current_scope(&self) -> Result<Arc<Self>, String> {
        let current = rpi_agent::tool_execution_scope::current_scope();
        if let Some(bound) = &self.scope {
            if !bound
                .upgrade()
                .zip(current)
                .is_some_and(|(bound, current)| {
                    Arc::ptr_eq(&bound, &current) && !bound.runtime_closing().is_cancelled()
                })
            {
                return Err("provider runtime belongs to another or closed child session".into());
            }
            return self
                .myself
                .upgrade()
                .ok_or("provider runtime expired".into());
        }
        let Some(scope) = current else {
            return self
                .myself
                .upgrade()
                .ok_or("provider runtime expired".into());
        };
        if scope.runtime_closing().is_cancelled() {
            return Err("child session provider runtime is closing".into());
        }
        let mut children = self.children.lock().await;
        children.retain(|_, child| {
            child
                .scope
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|scope| !scope.runtime_closing().is_cancelled())
        });
        if let Some(child) = children.get(scope.runtime_id()) {
            return Ok(child.clone());
        }
        if children.len() >= 64 {
            return Err("child provider runtime capacity exhausted".into());
        }
        let state = self.state.lock().await;
        let catalog = state.catalog.fork().map_err(|error| error.to_string())?;
        // Wrapper generations and future delegation admission belong to this
        // publication's execution scope, never to the parent's captured chain.
        let chains = Self::chains(&self.session, &catalog);
        let delegation_scope = DelegationScopeId::fresh();
        let epoch = DelegationEpoch::new(delegation_scope, chains.values().cloned());
        let child = Arc::new_cyclic(|myself| Self {
            delegation_scope,
            session: self.session.clone(),
            generation: self.generation.clone(),
            host: self.host.clone(),
            myself: myself.clone(),
            scope: Some(Arc::downgrade(&scope)),
            children: Mutex::new(BTreeMap::new()),
            state: Mutex::new(State {
                epoch,
                catalog,
                config: state.config.clone(),
                chains,
                generations: state.generations.clone(),
                refreshes: BTreeMap::new(),
            }),
            logins: Mutex::new(BTreeMap::new()),
            auth: self.auth.clone(),
            publications: Default::default(),
        });
        children.insert(scope.runtime_id().into(), child.clone());
        Ok(child)
    }
    fn validate_owner(&self, owner: &NativeOwner) -> Result<(), String> {
        if self.scope.as_ref().is_some_and(|scope| {
            scope
                .upgrade()
                .map_or(true, |scope| scope.runtime_closing().is_cancelled())
        }) {
            return Err("child session provider runtime is closing".into());
        }
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
        let mut owner = owner;
        owner.child = false; // Catalog ownership is inherited; each child has a separate registry.

        if signal.is_cancelled() {
            return Err("provider runtime request cancelled".into());
        }
        let action = args["action"].as_str().ok_or("missing provider action")?;
        if action == "inspect" {
            return Ok(
                json!({"revision":state.catalog.revision(),"providers":state.catalog.list_providers(),"available":state.catalog.list_providers().into_iter().filter(|provider| provider.auth_status == crate::model_registry::AuthStatus::Authenticated).flat_map(|provider| provider.models).collect::<Vec<_>>()}),
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
        if registered_owner.as_ref() != Some(&owner) {
            return Err("provider belongs to a different session or plugin".into());
        }
        let publication = if let Some(ticket) = args.get("publicationId") {
            let publication = self
                .publications
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(
                    ticket
                        .as_str()
                        .ok_or("invalid catalog publication identity")?,
                )
                .and_then(Weak::upgrade)
                .ok_or("expired catalog publication")?;
            if action != "publish"
                || publication.owner != owner
                || publication.provider != id
                || publication.committed.load(Ordering::Acquire) != 0
            {
                return Err("catalog publication does not own this update".into());
            }
            if publication.generation != state.generation(id) {
                return Err("provider catalog changed during the operation".into());
            }
            if publication.signal.is_cancelled() {
                return Err("provider catalog refresh cancelled".into());
            }
            Some(publication)
        } else {
            None
        };
        if args
            .get("expectedRevision")
            .is_some_and(|revision| revision.as_u64() != Some(state.catalog.revision()))
        {
            return Err("provider catalog changed during the operation".into());
        }
        let next_generation = if publication.is_none() {
            Some(state.next_generation(id)?)
        } else {
            None
        };
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
        if action != "unregister" {
            candidate
                .recompose_config(&state.config, Some(&[id.into()]))
                .map_err(|error| error.to_string())?;
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
        let epoch = DelegationEpoch::new(self.delegation_scope, chains.values().cloned());
        state.epoch.revoke();
        state.epoch = epoch;
        state.catalog = candidate;
        state.chains = chains;
        if let Some(generation) = next_generation {
            state.supersede(id, generation);
        }
        if let Some(publication) = publication {
            publication.committed.store(revision, Ordering::Release);
        }
        Ok(json!({"revision":revision}))
    }
    // Pi model-runtime.ts:848–888: config -> model snapshot -> callbacks ->
    // model snapshot -> availability. Callback publication still uses the same
    // single-use tickets and revokes the delegation epoch at every commit.
    async fn refresh(
        &self,
        caller: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let config = crate::config::load_models_config().map_err(|error| error.to_string())?;
        self.refresh_config(caller, args, signal, config).await
    }
    async fn refresh_config(
        &self,
        caller: NativeOwner,
        args: Value,
        signal: CancellationToken,
        config: crate::config::ModelsConfig,
    ) -> Result<Value, String> {
        self.validate_owner(&caller)?;
        let mut selected: Option<Vec<String>> = if let Some(id) = args.get("providerId") {
            Some(vec![id.as_str().ok_or("invalid provider identity")?.into()])
        } else {
            args.get("providers")
                .map(|ids| serde_json::from_value(ids.clone()))
                .transpose()
                .map_err(|_| "invalid provider selection")?
        };
        if let Some(ids) = &mut selected {
            let mut seen = std::collections::BTreeSet::new();
            ids.retain(|id| seen.insert(id.clone()));
        }
        let ids = {
            let mut state = self.state.lock().await;
            self.validate_owner(&caller)?;
            if signal.is_cancelled() {
                return Err("provider runtime request cancelled".into());
            }
            let candidate = state.catalog.fork().map_err(|error| error.to_string())?;
            candidate
                .recompose_config(&config, selected.as_deref())
                .map_err(|error| error.to_string())?;
            let ids: Vec<String> = selected.clone().unwrap_or_else(|| {
                candidate
                    .list_providers()
                    .into_iter()
                    .map(|provider| provider.id)
                    .collect()
            });
            // Reserve all generations before awaiting any callback, including an
            // overlapping provider-local pass that is still in flight.
            let generations = ids
                .iter()
                .map(|id| {
                    state
                        .next_generation(id)
                        .map(|generation| (id.clone(), generation))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if candidate.revision() != state.catalog.revision() {
                let chains = Self::chains(&self.session, &candidate);
                self.host
                    .install_native_providers(self.proxies(&candidate))
                    .await?;
                self.host.replace_model_catalog(candidate.list_models());
                state.epoch.revoke();
                state.epoch = DelegationEpoch::new(self.delegation_scope, chains.values().cloned());
                state.catalog = candidate;
                state.chains = chains;
            }
            if let Some(selected) = &selected {
                for id in selected {
                    match config.providers.get(id) {
                        Some(provider) => {
                            state.config.providers.insert(id.clone(), provider.clone());
                        }
                        None => {
                            state.config.providers.shift_remove(id);
                        }
                    }
                }
            } else {
                state.config = config;
            }
            for (id, generation) in &generations {
                state.supersede(id, *generation);
            }
            generations
        };
        let mut errors = BTreeMap::new();
        let mut revision = self.state.lock().await.catalog.revision();
        for (id, generation) in ids {
            if signal.is_cancelled() {
                break;
            }
            let supported =
                rpi_extensions::provider_runtime::NativeProviderOperations::from_session(
                    &self.session,
                    &id,
                )
                .is_some_and(|operations| operations.supports("refreshCatalog"));
            if !supported {
                continue;
            }
            let mut input = args.clone();
            input["providerId"] = json!(id);
            input["refreshGeneration"] = json!(generation);
            match self
                .refresh_provider(caller.clone(), input, signal.clone())
                .await
            {
                Ok(result) => revision = result["revision"].as_u64().unwrap_or(revision),
                Err(error) if args.get("providerId").is_some() => return Err(error),
                Err(error) => {
                    errors.insert(id, error);
                }
            }
        }
        self.refresh_availability(caller, selected.as_deref(), signal.clone())
            .await?;
        if args.get("providerId").is_some() {
            Ok(json!({"revision":revision}))
        } else {
            Ok(json!({"revision":revision,"aborted":signal.is_cancelled(),"errors":errors}))
        }
    }
    async fn refresh_provider(
        &self,
        caller: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let id = args["providerId"]
            .as_str()
            .ok_or("missing provider identity")?;
        self.validate_owner(&caller)?;
        if signal.is_cancelled() {
            return Err("provider runtime request cancelled".into());
        }
        let operations = rpi_extensions::provider_runtime::NativeProviderOperations::from_session(
            &self.session,
            id,
        )
        .ok_or("provider has no catalog refresh callback")?;
        let ticket = uuid::Uuid::new_v4().to_string();
        let publication = {
            let mut state = self.state.lock().await;
            self.validate_owner(&caller)?;
            if signal.is_cancelled() {
                return Err("provider runtime request cancelled".into());
            }
            if args["refreshGeneration"]
                .as_u64()
                .is_some_and(|generation| generation != state.generation(id))
            {
                return Err("provider catalog changed during the operation".into());
            }
            let owner = state
                .catalog
                .owner(id)
                .ok_or("provider has no native catalog owner")?;
            let generation = state.next_generation(id)?;
            let mut pending = self.publications.lock().unwrap_or_else(|e| e.into_inner());
            pending.retain(|_, publication| publication.strong_count() != 0);
            if pending.len() >= 64 {
                return Err("catalog refresh capacity exhausted".into());
            }
            let publication = Arc::new(Publication {
                owner,
                provider: id.into(),
                generation,
                signal: signal.child_token(),
                committed: AtomicU64::new(0),
            });
            state.supersede(id, generation);
            state
                .refreshes
                .insert(id.into(), Arc::downgrade(&publication));
            pending.insert(ticket.clone(), Arc::downgrade(&publication));
            publication
        };
        let result = operations.request("refreshCatalog", json!({"allowNetwork":args["allowNetwork"].as_bool().unwrap_or(false),"force":args["force"].as_bool().unwrap_or(false),"publicationId":ticket}), publication.signal.clone()).await;
        if self.state.lock().await.generation(id) != publication.generation {
            return Err("provider catalog changed during the operation".into());
        }
        let result = result?;
        let committed = publication.committed.load(Ordering::Acquire);
        if committed != 0 || result.get("published").is_some() {
            if committed == 0 || result != json!({"published":true}) {
                return Err("provider did not acknowledge its catalog publication".into());
            }
            return Ok(json!({"revision":committed}));
        }
        self.catalog_request(publication.owner.clone(), json!({"action":"publish","providerId":id,"models":result["models"],"publicationId":ticket}), signal).await
    }
    async fn refresh_availability(
        &self,
        owner: NativeOwner,
        selected: Option<&[String]>,
        signal: CancellationToken,
    ) -> Result<(), String> {
        let (ticket, ids) = {
            let state = self.state.lock().await;
            let ticket = state
                .catalog
                .begin_availability(selected)
                .map_err(|error| error.to_string())?;
            let ids: Vec<String> = selected.map_or_else(
                || {
                    state
                        .catalog
                        .list_providers()
                        .into_iter()
                        .map(|provider| provider.id)
                        .collect()
                },
                |ids| ids.to_vec(),
            );
            (ticket, ids)
        };
        let mut statuses = Vec::new();
        for id in ids {
            // Status reads only local credential storage: availability must not
            // resolve/refresh OAuth or hand private request headers to a catalog.
            let status = self
                .auth_request(
                    owner.clone(),
                    json!({"action":"status","providerId":id}),
                    signal.clone(),
                )
                .await;
            let status = match status {
                Ok(status) if status["refreshRequired"] == true => {
                    crate::model_registry::AuthStatus::Expired
                }
                Ok(status) if status["present"] == true => {
                    crate::model_registry::AuthStatus::Authenticated
                }
                Ok(_) => crate::model_registry::AuthStatus::Unauthenticated,
                Err(_) => crate::model_registry::AuthStatus::Unknown,
            };
            statuses.push((id, status));
        }
        if !signal.is_cancelled() {
            self.validate_owner(&owner)?;
            self.state
                .lock()
                .await
                .catalog
                .finish_availability(ticket, statuses)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
    async fn invalidate_credentials(&self, selected: Option<&[String]>) -> Result<(), String> {
        let state = self.state.lock().await;
        state
            .catalog
            .begin_availability(selected)
            .map_err(|error| error.to_string())?;
        let ids: Vec<_> = selected.map_or_else(
            || {
                state
                    .catalog
                    .list_providers()
                    .into_iter()
                    .map(|provider| provider.id)
                    .collect()
            },
            |ids| ids.to_vec(),
        );
        for id in ids {
            if state.catalog.has_provider(&id) {
                state
                    .catalog
                    .update_auth_status(&id, crate::model_registry::AuthStatus::Unknown)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
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
        if owner.child != rpi_agent::tool_execution_scope::is_child_execution() {
            return Err(
                "child session ownership does not match the current execution scope".into(),
            );
        }
        let runtime = self.for_current_scope().await?;
        runtime.validate_owner(&owner)?;
        match args["port"].as_str() {
            Some("providers") if args["action"] == "refresh" => {
                runtime.refresh(owner, args, signal).await
            }
            Some("providers") => runtime.catalog_request(owner, args, signal).await,
            Some("auth") => {
                let mutates = matches!(
                    args["action"].as_str(),
                    Some("login" | "continue" | "refresh" | "logout" | "credentialStore")
                );
                if !mutates {
                    return runtime.auth_request(owner, args, signal).await;
                }
                let selected = args["providerId"].as_str().map(|id| vec![id.to_string()]);
                runtime.invalidate_credentials(selected.as_deref()).await?;
                let result = runtime
                    .auth_request(owner.clone(), args, signal.clone())
                    .await;
                // Invalidate again after writes settle, even on error/cancellation:
                // a status pass may have read the pre-write credential meanwhile.
                runtime.invalidate_credentials(selected.as_deref()).await?;
                if result.is_ok() {
                    runtime
                        .refresh_availability(owner, selected.as_deref(), signal)
                        .await?;
                }
                result
            }
            _ => Err("unsupported native runtime port".into()),
        }
    }
}
impl Drop for ProviderRuntime {
    fn drop(&mut self) {
        self.state.get_mut().epoch.revoke();
    }
}
#[async_trait::async_trait]
impl DelegationResolver for ProviderRuntime {
    async fn resolve_target(
        &self,
        caller: &DelegationCaller,
        provider_id: &str,
        model_id: &str,
        signal: CancellationToken,
    ) -> Result<ResolvedDelegationTarget, String> {
        caller.validate()?;
        if signal.is_cancelled()
            || self.scope.as_ref().is_some_and(|scope| {
                scope
                    .upgrade()
                    .map_or(true, |scope| scope.runtime_closing().is_cancelled())
            })
            || !self
                .session
                .snapshot()
                .is_some_and(|snapshot| snapshot.is_active())
        {
            return Err("delegation epoch revoked".into());
        }
        self.state
            .lock()
            .await
            .epoch
            .target(caller, provider_id, model_id)
    }
    async fn resolve_ambient(
        &self,
        caller: &DelegationCaller,
        target: &ResolvedDelegationTarget,
        signal: CancellationToken,
    ) -> Result<ResolvedAmbientAuth, String> {
        target.validate(caller)?;
        let provider = {
            let state = self.state.lock().await;
            // Revalidate against the currently published scope, not only a retained Arc.
            state
                .epoch
                .target(caller, &target.model().provider, &target.model().id)?;
            state
                .catalog
                .get_provider_instance(&target.model().provider)
                .ok_or("provider is no longer registered")?
        };
        let revoked = target.signal();
        let authentication = tokio::select! { biased;
            _ = revoked.cancelled() => return Err("delegation epoch revoked".into()),
            _ = signal.cancelled() => return Err("authentication cancelled".into()),
            result = provider.delegation_auth(target.model(), signal.clone()) => result?,
        };
        target.validate(caller)?;
        if signal.is_cancelled()
            || !self
                .session
                .snapshot()
                .is_some_and(|snapshot| snapshot.is_active())
        {
            return Err("authentication cancelled".into());
        }
        Ok(ResolvedAmbientAuth::new(authentication))
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
        let runtime = match self.runtime.for_current_scope().await {
            Ok(runtime) => runtime,
            Err(error) => return error_stream(model, error).await,
        };
        let (provider, epoch) = {
            let state = runtime.state.lock().await;
            (state.chains.get(&self.id).cloned(), state.epoch.clone())
        };
        let resolver: Arc<dyn DelegationResolver> = runtime.clone();
        let binding =
            match DelegationBinding::root(Arc::downgrade(&resolver), epoch.clone(), &self.id) {
                Ok(binding) => binding,
                Err(error) => return error_stream(model, error).await,
            };
        match provider {
            Some(provider) => delegation::forward_scoped(
                runtime, provider, binding, epoch, model, context, options,
            ),
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
#[path = "provider_runtime/catalog_lifecycle_tests.rs"]
mod catalog_lifecycle_tests;
#[cfg(test)]
mod tests;
