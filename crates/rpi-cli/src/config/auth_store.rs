//! Shared credential-store lock for login, logout and compatibility refresh.

use super::{atomic_write, ensure_dir, AuthStore, ConfigError};
use fs2::FileExt;
use rpi_ai::auth::{AuthError, Credential, CredentialStore, CredentialTransaction};
use serde_json::{Map, Value};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Explicit path ownership prevents an environment change during a refresh
/// from publishing the rotated token into another account store.
pub struct CompatibilityCredentialStore {
    path: PathBuf,
}

impl CompatibilityCredentialStore {
    pub fn new(path: PathBuf) -> Result<Self, AuthError> {
        if !path.is_absolute() {
            return Err(AuthError::Store);
        }
        Ok(Self { path })
    }
}

fn read(path: &Path) -> Result<Map<String, Value>, ConfigError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| ConfigError::Json {
            path: path.into(),
            source,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(source) => Err(ConfigError::Read {
            path: path.into(),
            source,
        }),
    }
}

struct LockedStore {
    path: PathBuf,
    lock: File,
    values: Map<String, Value>,
}

impl Drop for LockedStore {
    fn drop(&mut self) {
        // A concurrently spawned child may retain a descriptor until exec.
        let _ = FileExt::unlock(&self.lock);
    }
}

impl LockedStore {
    fn acquire(path: PathBuf, signal: &CancellationToken) -> Result<Self, ConfigError> {
        let mut store = Self::acquire_file(path, signal, None)?;
        store.values = read(&store.path)?;
        Ok(store)
    }

    fn acquire_file(
        path: PathBuf,
        signal: &CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Self, ConfigError> {
        let failure = |source| ConfigError::Write {
            path: path.clone(),
            source,
        };
        let parent = path.parent().ok_or_else(|| {
            failure(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "credential path has no parent",
            ))
        })?;
        ensure_dir(parent)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(path.with_extension("json.lock"))
            .map_err(failure)?;
        loop {
            if signal.is_cancelled() {
                return Err(failure(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "credential lock cancelled",
                )));
            }
            match lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                        return Err(failure(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "credential store lock timed out",
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => return Err(failure(error)),
            }
        }
        Ok(Self {
            path,
            lock,
            values: Map::new(),
        })
    }

    fn commit(&self) -> Result<(), ConfigError> {
        let bytes =
            serde_json::to_vec_pretty(&self.values).map_err(|source| ConfigError::Json {
                path: self.path.clone(),
                source,
            })?;
        atomic_write(&self.path, &bytes)?;
        #[cfg(unix)]
        File::open(self.path.parent().unwrap())
            .and_then(|directory| directory.sync_all())
            .map_err(|source| ConfigError::Write {
                path: self.path.clone(),
                source,
            })?;
        Ok(())
    }
}

/// An owning native provider may preserve raw credential bytes while holding
/// the same lock as login, logout and refresh. The host supplies the path.
pub(crate) struct CredentialFileLease(LockedStore);
impl CredentialFileLease {
    pub(crate) fn path(&self) -> &Path {
        &self.0.path
    }
}
impl CompatibilityCredentialStore {
    pub(crate) async fn lock_file(
        &self,
        signal: CancellationToken,
        timeout: std::time::Duration,
    ) -> Result<CredentialFileLease, AuthError> {
        let signal = signal.child_token();
        let _cancel_wait = signal.clone().drop_guard();
        let path = self.path.clone();
        let deadline = std::time::Instant::now() + timeout;
        tokio::task::spawn_blocking(move || {
            LockedStore::acquire_file(path, &signal, Some(deadline)).map(CredentialFileLease).map_err(|error| {
                if signal.is_cancelled() { AuthError::Cancelled }
                else if matches!(error,ConfigError::Write{source,..} if source.kind()==std::io::ErrorKind::TimedOut) { AuthError::Provider("credential store lock timed out".into()) }
                else { AuthError::Store }
            })
        }).await.map_err(|_| AuthError::Task)?
    }
}

pub(super) fn write(path: PathBuf, values: &AuthStore) -> Result<(), ConfigError> {
    let mut store = LockedStore::acquire(path.clone(), &CancellationToken::new())?;
    store.values = serde_json::to_value(values)
        .map_err(|source| ConfigError::Json { path, source })?
        .as_object()
        .cloned()
        .expect("AuthStore serializes as an object");
    store.commit()
}

pub(super) fn upsert(
    path: PathBuf,
    provider: &str,
    value: &super::Credential,
) -> Result<(), ConfigError> {
    let mut store = LockedStore::acquire(path.clone(), &CancellationToken::new())?;
    let value = serde_json::to_value(value).map_err(|source| ConfigError::Json { path, source })?;
    store.values.insert(provider.into(), value);
    store.commit()
}

pub(super) fn delete(path: PathBuf, provider: &str) -> Result<bool, ConfigError> {
    let mut store = LockedStore::acquire(path, &CancellationToken::new())?;
    if store.values.remove(provider).is_none() {
        return Ok(false);
    }
    store.commit()?;
    Ok(true)
}

fn from_legacy(value: Option<&Value>) -> Result<Option<Credential>, AuthError> {
    let Some(mut value) = value.cloned() else {
        return Ok(None);
    };
    if value.get("type").and_then(Value::as_str) == Some("oauth") {
        let seconds = match value.get("expires") {
            None => 0,
            Some(value) => value.as_i64().ok_or(AuthError::Store)?,
        };
        let milliseconds = seconds.checked_mul(1000).ok_or(AuthError::Store)?;
        value["expires"] = milliseconds.into();
    }
    if value.get("env") == Some(&Value::Null) {
        value.as_object_mut().ok_or(AuthError::Store)?.remove("env");
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|_| AuthError::Store)
}

fn to_legacy(credential: &Credential) -> Result<Value, AuthError> {
    let mut value = serde_json::to_value(credential).map_err(|_| AuthError::Store)?;
    if let Credential::Oauth(credential) = credential {
        // Round down: conversion must never extend a token's actual lifetime.
        value["expires"] = credential.expires_ms.div_euclid(1000).into();
    }
    Ok(value)
}

struct Transaction {
    store: Option<LockedStore>,
    provider: String,
    current: Option<Credential>,
}

impl Transaction {
    async fn commit(&mut self, credential: Option<Credential>) -> Result<(), AuthError> {
        let value = credential.as_ref().map(to_legacy).transpose()?;
        let mut store = self.store.take().ok_or(AuthError::Store)?;
        match value {
            Some(value) => {
                store.values.insert(self.provider.clone(), value);
            }
            None => {
                store.values.remove(&self.provider);
            }
        }
        // The blocking task owns the lock, even if this future is dropped.
        let (store, result) = tokio::task::spawn_blocking(move || {
            let result = store.commit();
            (store, result)
        })
        .await
        .map_err(|_| AuthError::Task)?;
        self.store = Some(store);
        result.map_err(|_| AuthError::Store)?;
        self.current = credential;
        Ok(())
    }
}

#[async_trait::async_trait]
impl CredentialTransaction for Transaction {
    fn current(&self) -> Option<Credential> {
        self.current.clone()
    }

    async fn replace(&mut self, credential: Credential) -> Result<(), AuthError> {
        self.commit(Some(credential)).await
    }
    async fn remove(&mut self) -> Result<(), AuthError> {
        self.commit(None).await
    }
}

#[async_trait::async_trait]
impl CredentialStore for CompatibilityCredentialStore {
    async fn read(&self, provider: &str) -> Result<Option<Credential>, AuthError> {
        let path = self.path.clone();
        let provider = provider.to_owned();
        tokio::task::spawn_blocking(move || {
            let values = read(&path).map_err(|_| AuthError::Store)?;
            let mut credential = from_legacy(values.get(&provider))?;
            if let Some(Credential::ApiKey { key, env }) = &mut credential {
                *key = key
                    .as_deref()
                    .and_then(|key| super::resolve_config_value(key, Some(env)));
            }
            Ok(credential)
        })
        .await
        .map_err(|_| AuthError::Task)?
    }

    async fn lock(
        &self,
        provider: &str,
        signal: CancellationToken,
    ) -> Result<Box<dyn CredentialTransaction>, AuthError> {
        let signal = signal.child_token();
        let _cancel_wait = signal.clone().drop_guard();
        let path = self.path.clone();
        let provider = provider.to_owned();
        tokio::task::spawn_blocking(move || {
            let store = LockedStore::acquire(path, &signal).map_err(|_| {
                if signal.is_cancelled() {
                    AuthError::Cancelled
                } else {
                    AuthError::Store
                }
            })?;
            let current = from_legacy(store.values.get(&provider))?;
            Ok(Box::new(Transaction {
                store: Some(store),
                provider,
                current,
            }) as Box<dyn CredentialTransaction>)
        })
        .await
        .map_err(|_| AuthError::Task)?
    }
}

#[cfg(test)]
mod tests;
