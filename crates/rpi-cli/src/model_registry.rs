//! Atomic provider catalog and execution snapshots, scoped to native owners.
use rpi_ai::{Model, Provider};
use rpi_extensions::native::NativeOwner;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum ModelRegistryEvent {
    ProviderRegistered {
        provider_id: String,
    },
    ProviderUnregistered {
        provider_id: String,
    },
    CatalogRefreshed,
    AuthStatusChanged {
        provider_id: String,
        authenticated: bool,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthStatus {
    Unauthenticated,
    Authenticated,
    Expired,
    Unknown,
}
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    pub auth_status: AuthStatus,
    pub models: Vec<Model>,
}
#[derive(Clone)]
struct Entry {
    info: ProviderInfo,
    provider: Option<Arc<dyn Provider>>,
    owner: Option<NativeOwner>,
    // Keep the pre-config catalog so deleted overrides cannot linger.
    config_base: Option<Vec<Model>>,
    config_only: bool,
}
#[derive(Clone, Default)]
struct State {
    entries: Vec<Entry>,
    revision: u64,
    last_refresh: Option<std::time::Instant>,
    availability_sequence: u64,
    provider_sequences: std::collections::BTreeMap<String, u64>,
}
/// An availability result is valid only for this catalog and credential pass.
#[derive(Clone)]
pub(crate) struct AvailabilityTicket {
    revision: u64,
    global: u64,
    providers: std::collections::BTreeMap<String, u64>,
}
pub struct ModelRegistry {
    state: RwLock<State>,
    event_tx: broadcast::Sender<ModelRegistryEvent>,
}
impl ModelRegistry {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(100);
        Self {
            state: RwLock::new(State::default()),
            event_tx,
        }
    }
    fn register(&self, mut entry: Entry, enforce_owner: bool) -> Result<u64, RegistryError> {
        let id = entry.info.id.clone();
        if id.trim().is_empty() || entry.provider.as_ref().is_some_and(|p| p.id() != id) {
            return Err(RegistryError::InvalidConfiguration(
                "provider identity mismatch".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        if entry.info.models.iter().any(|model| {
            model.provider != id
                || model.id.trim().is_empty()
                || !seen.insert((model.api.clone(), model.id.to_ascii_lowercase()))
        }) {
            return Err(RegistryError::InvalidConfiguration(
                "invalid or duplicate model identity".into(),
            ));
        }
        for model in &mut entry.info.models {
            model.headers = None;
        }
        let mut state = self
            .state
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        let revision = state
            .revision
            .checked_add(1)
            .ok_or(RegistryError::RevisionExhausted)?;
        if let Some(existing) = state.entries.iter_mut().find(|known| known.info.id == id) {
            if enforce_owner && existing.owner != entry.owner {
                return Err(RegistryError::WrongOwner(id));
            }
            *existing = entry;
        } else {
            state.entries.push(entry);
        }
        state.revision = revision;
        drop(state);
        let _ = self
            .event_tx
            .send(ModelRegistryEvent::ProviderRegistered { provider_id: id });
        Ok(revision)
    }
    pub fn register_builtin_provider(
        &self,
        id: String,
        name: String,
        models: Vec<Model>,
    ) -> Result<(), RegistryError> {
        self.register(
            Entry {
                info: ProviderInfo {
                    id,
                    name,
                    builtin: true,
                    auth_status: AuthStatus::Unknown,
                    models,
                },
                provider: None,
                owner: None,
                config_base: None,
                config_only: false,
            },
            false,
        )
        .map(|_| ())
    }
    pub fn register_extension_provider(
        &self,
        provider: Arc<dyn Provider>,
    ) -> Result<(), RegistryError> {
        self.publish(None, provider, false, false).map(|_| ())
    }
    fn publish(
        &self,
        owner: Option<NativeOwner>,
        provider: Arc<dyn Provider>,
        enforce_owner: bool,
        builtin: bool,
    ) -> Result<u64, RegistryError> {
        self.register(
            Entry {
                info: ProviderInfo {
                    id: provider.id().into(),
                    name: provider.id().into(),
                    builtin,
                    auth_status: AuthStatus::Unknown,
                    models: provider.models().to_vec(),
                },
                provider: Some(provider),
                owner,
                config_base: None,
                config_only: false,
            },
            enforce_owner,
        )
    }
    pub fn publish_owned(
        &self,
        owner: NativeOwner,
        provider: Arc<dyn Provider>,
    ) -> Result<u64, RegistryError> {
        self.publish(Some(owner), provider, true, false)
    }
    pub(crate) fn register_discovered(
        &self,
        owner: Option<NativeOwner>,
        id: String,
        models: Vec<Model>,
    ) -> Result<(), RegistryError> {
        let builtin = owner.is_none();
        self.register(
            Entry {
                info: ProviderInfo {
                    name: id.clone(),
                    id,
                    builtin,
                    auth_status: AuthStatus::Unknown,
                    models,
                },
                provider: None,
                owner,
                config_base: None,
                config_only: false,
            },
            true,
        )
        .map(|_| ())
    }
    pub(crate) fn register_runtime(
        &self,
        owner: Option<NativeOwner>,
        provider: Arc<dyn Provider>,
    ) -> Result<u64, RegistryError> {
        let builtin = owner.is_none();
        self.publish(owner, provider, false, builtin)
    }
    pub(crate) fn fork(&self) -> Result<Self, RegistryError> {
        let candidate = Self::new();
        *candidate
            .state
            .write()
            .map_err(|_| RegistryError::LockPoisoned)? = self
            .state
            .read()
            .map_err(|_| RegistryError::LockPoisoned)?
            .clone();
        Ok(candidate)
    }
    pub fn unregister_owned(&self, owner: &NativeOwner, id: &str) -> Result<u64, RegistryError> {
        self.remove(Some(owner), id)
    }
    pub fn unregister_provider(&self, id: &str) -> Result<(), RegistryError> {
        self.remove(None, id).map(|_| ())
    }
    fn remove(&self, owner: Option<&NativeOwner>, id: &str) -> Result<u64, RegistryError> {
        let mut state = self
            .state
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        let index = state
            .entries
            .iter()
            .position(|entry| entry.info.id == id)
            .ok_or_else(|| RegistryError::ProviderNotFound(id.into()))?;
        if owner.is_some() && state.entries[index].owner.as_ref() != owner {
            return Err(RegistryError::WrongOwner(id.into()));
        }
        let revision = state
            .revision
            .checked_add(1)
            .ok_or(RegistryError::RevisionExhausted)?;
        state.entries.remove(index);
        state.revision = revision;
        drop(state);
        let _ = self
            .event_tx
            .send(ModelRegistryEvent::ProviderUnregistered {
                provider_id: id.into(),
            });
        Ok(revision)
    }
    pub fn owner(&self, id: &str) -> Option<NativeOwner> {
        self.state
            .read()
            .ok()?
            .entries
            .iter()
            .find(|entry| entry.info.id == id)?
            .owner
            .clone()
    }
    pub fn revision(&self) -> u64 {
        self.state.read().expect("model registry lock").revision
    }
    pub fn get_provider(&self, id: &str) -> Option<ProviderInfo> {
        self.state
            .read()
            .ok()?
            .entries
            .iter()
            .find(|entry| entry.info.id == id)
            .map(|entry| entry.info.clone())
    }
    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        self.state
            .read()
            .expect("model registry lock")
            .entries
            .iter()
            .map(|entry| entry.info.clone())
            .collect()
    }
    pub fn list_models(&self) -> Vec<Model> {
        self.list_providers()
            .into_iter()
            .flat_map(|entry| entry.models)
            .collect()
    }
    pub fn list_provider_models(&self, id: &str) -> Vec<Model> {
        self.get_provider(id)
            .map(|entry| entry.models)
            .unwrap_or_default()
    }
    pub fn update_auth_status(&self, id: &str, status: AuthStatus) -> Result<(), RegistryError> {
        let mut state = self
            .state
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        let entry = state
            .entries
            .iter_mut()
            .find(|entry| entry.info.id == id)
            .ok_or_else(|| RegistryError::ProviderNotFound(id.into()))?;
        entry.info.auth_status = status.clone();
        state.availability_sequence = state
            .availability_sequence
            .checked_add(1)
            .ok_or(RegistryError::RevisionExhausted)?;
        let sequence = state.provider_sequences.entry(id.into()).or_default();
        *sequence = sequence
            .checked_add(1)
            .ok_or(RegistryError::RevisionExhausted)?;
        drop(state);
        let _ = self.event_tx.send(ModelRegistryEvent::AuthStatusChanged {
            provider_id: id.into(),
            authenticated: status == AuthStatus::Authenticated,
        });
        Ok(())
    }
    pub(crate) fn begin_availability(
        &self,
        selected: Option<&[String]>,
    ) -> Result<AvailabilityTicket, RegistryError> {
        let mut state = self
            .state
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        state.availability_sequence = state
            .availability_sequence
            .checked_add(1)
            .ok_or(RegistryError::RevisionExhausted)?;
        let ids: Vec<String> = selected.map_or_else(
            || {
                state
                    .entries
                    .iter()
                    .map(|entry| entry.info.id.clone())
                    .collect()
            },
            |ids| ids.to_vec(),
        );
        let mut providers = std::collections::BTreeMap::new();
        for id in ids {
            let sequence = state.provider_sequences.entry(id).or_default();
            *sequence = sequence
                .checked_add(1)
                .ok_or(RegistryError::RevisionExhausted)?;
        }
        for (id, sequence) in &state.provider_sequences {
            if selected.map_or(true, |ids| ids.contains(id)) {
                providers.insert(id.clone(), *sequence);
            }
        }
        Ok(AvailabilityTicket {
            revision: state.revision,
            global: if selected.is_none() {
                state.availability_sequence
            } else {
                0
            },
            providers,
        })
    }
    pub(crate) fn finish_availability(
        &self,
        ticket: AvailabilityTicket,
        statuses: Vec<(String, AuthStatus)>,
    ) -> Result<bool, RegistryError> {
        let mut state = self
            .state
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        if state.revision != ticket.revision
            || (ticket.global != 0 && ticket.global != state.availability_sequence)
        {
            return Ok(false);
        }
        let mut applied = false;
        for (id, status) in statuses {
            if ticket.providers.get(&id).is_none()
                || ticket.providers.get(&id) != state.provider_sequences.get(&id)
            {
                continue;
            }
            if let Some(entry) = state.entries.iter_mut().find(|entry| entry.info.id == id) {
                entry.info.auth_status = status;
                applied = true;
            }
        }
        Ok(applied)
    }
    pub fn get_auth_status(&self, id: &str) -> Option<AuthStatus> {
        self.get_provider(id).map(|entry| entry.auth_status)
    }
    pub async fn refresh_catalog(&self) -> Result<(), RegistryError> {
        let config = crate::config::load_models_config()
            .map_err(|error| RegistryError::InvalidConfiguration(error.to_string()))?;
        let mut state = self
            .state
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        Self::recompose(&mut state, &config, None)?;
        drop(state);
        let _ = self.event_tx.send(ModelRegistryEvent::CatalogRefreshed);
        Ok(())
    }
    pub(crate) fn recompose_config(
        &self,
        config: &crate::config::ModelsConfig,
        selected: Option<&[String]>,
    ) -> Result<(), RegistryError> {
        let mut state = self
            .state
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        Self::recompose(&mut state, config, selected)
    }
    fn recompose(
        state: &mut State,
        config: &crate::config::ModelsConfig,
        selected: Option<&[String]>,
    ) -> Result<(), RegistryError> {
        let chosen = |id: &str| selected.map_or(true, |ids| ids.iter().any(|known| known == id));
        if !config.providers.keys().any(|id| chosen(id))
            && !state.entries.iter().any(|entry| {
                chosen(&entry.info.id) && (entry.config_base.is_some() || entry.config_only)
            })
        {
            return Ok(());
        }
        let mut candidate = state.clone();
        candidate
            .entries
            .retain(|entry| !chosen(&entry.info.id) || !entry.config_only);
        for entry in &mut candidate.entries {
            if chosen(&entry.info.id) {
                if let Some(models) = entry.config_base.take() {
                    Self::replace_models(entry, models)?;
                }
            }
        }
        for (id, config) in &config.providers {
            if !chosen(id) {
                continue;
            }
            let models = crate::config::provider_to_models(id, config).unwrap_or_default();
            if let Some(entry) = candidate
                .entries
                .iter_mut()
                .find(|entry| &entry.info.id == id)
            {
                entry.config_base = Some(entry.info.models.clone());
                let mut merged = entry.info.models.clone();
                for model in models {
                    if let Some(existing) = merged.iter_mut().find(|known| {
                        known.api == model.api && known.id.eq_ignore_ascii_case(&model.id)
                    }) {
                        *existing = model;
                    } else {
                        merged.push(model);
                    }
                }
                Self::replace_models(entry, merged)?;
            } else {
                candidate.entries.push(Entry {
                    info: ProviderInfo {
                        id: id.clone(),
                        name: config.name.clone().unwrap_or_else(|| id.clone()),
                        builtin: false,
                        auth_status: AuthStatus::Unknown,
                        models,
                    },
                    provider: None,
                    owner: None,
                    config_base: None,
                    config_only: true,
                });
            }
        }
        let revision = state
            .revision
            .checked_add(1)
            .ok_or(RegistryError::RevisionExhausted)?;
        for entry in &mut candidate.entries {
            for model in &mut entry.info.models {
                model.headers = None;
            }
        }
        candidate.revision = revision;
        candidate.last_refresh = Some(std::time::Instant::now());
        *state = candidate;
        Ok(())
    }
    fn replace_models(entry: &mut Entry, mut models: Vec<Model>) -> Result<(), RegistryError> {
        for model in &mut models {
            model.headers = None;
        }
        if let Some(provider) = &entry.provider {
            let replacement = provider.with_catalog(models).ok_or_else(|| {
                RegistryError::InvalidConfiguration(
                    "provider cannot refresh its execution catalog".into(),
                )
            })?;
            entry.info.models = replacement.models().to_vec();
            entry.provider = Some(replacement);
        } else {
            entry.info.models = models;
        }
        Ok(())
    }
    pub fn last_refresh_time(&self) -> Option<std::time::Instant> {
        self.state.read().ok()?.last_refresh
    }
    pub fn subscribe(&self) -> broadcast::Receiver<ModelRegistryEvent> {
        self.event_tx.subscribe()
    }
    pub fn get_provider_instance(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.state
            .read()
            .ok()?
            .entries
            .iter()
            .find(|entry| entry.info.id == id)?
            .provider
            .clone()
    }
    pub fn instances(&self) -> Vec<Arc<dyn Provider>> {
        self.state
            .read()
            .expect("model registry lock")
            .entries
            .iter()
            .filter_map(|entry| entry.provider.clone())
            .collect()
    }
    pub fn has_provider(&self, id: &str) -> bool {
        self.get_provider(id).is_some()
    }
    pub fn provider_count(&self) -> usize {
        self.state
            .read()
            .expect("model registry lock")
            .entries
            .len()
    }
    pub fn model_count(&self) -> usize {
        self.state
            .read()
            .expect("model registry lock")
            .entries
            .iter()
            .map(|entry| entry.info.models.len())
            .sum()
    }
}
impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("model registry lock poisoned")]
    LockPoisoned,
    #[error("provider not found: {0}")]
    ProviderNotFound(String),
    #[error("provider already registered: {0}")]
    ProviderAlreadyRegistered(String),
    #[error("invalid provider configuration: {0}")]
    InvalidConfiguration(String),
    #[error("provider belongs to a different session or plugin: {0}")]
    WrongOwner(String),
    #[error("model registry revision exhausted")]
    RevisionExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_creation() {
        let registry = ModelRegistry::new();
        assert_eq!(registry.provider_count(), 0);
        assert_eq!(registry.model_count(), 0);
    }

    #[test]
    fn test_provider_registration() {
        let registry = ModelRegistry::new();

        assert!(registry
            .register_builtin_provider("test".to_string(), "Test Provider".to_string(), vec![])
            .is_ok());
        assert_eq!(registry.provider_count(), 1);
        assert!(registry.has_provider("test"));
    }

    #[test]
    fn test_provider_unregistration() {
        let registry = ModelRegistry::new();

        registry
            .register_builtin_provider("test".to_string(), "Test Provider".to_string(), vec![])
            .unwrap();
        assert!(registry.unregister_provider("test").is_ok());
        assert_eq!(registry.provider_count(), 0);
    }

    #[test]
    fn revision_exhaustion_never_changes_a_catalog() {
        let registry = ModelRegistry::new();
        registry
            .register_builtin_provider("first".into(), "First".into(), vec![])
            .unwrap();
        registry.state.write().unwrap().revision = u64::MAX;
        assert!(matches!(
            registry.register_builtin_provider("second".into(), "Second".into(), vec![]),
            Err(RegistryError::RevisionExhausted)
        ));
        assert!(matches!(
            registry.unregister_provider("first"),
            Err(RegistryError::RevisionExhausted)
        ));
        assert!(registry.has_provider("first"));
        assert!(!registry.has_provider("second"));
        assert_eq!(registry.provider_count(), 1);
    }

    #[test]
    fn test_auth_status_update() {
        let registry = ModelRegistry::new();

        registry
            .register_builtin_provider("test".to_string(), "Test Provider".to_string(), vec![])
            .unwrap();

        assert!(registry
            .update_auth_status("test", AuthStatus::Authenticated)
            .is_ok());
        assert_eq!(
            registry.get_auth_status("test"),
            Some(AuthStatus::Authenticated)
        );
    }
}
