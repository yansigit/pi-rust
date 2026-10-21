use super::*;
use rpi_ai::auth::OAuthCredential;
use std::collections::BTreeMap;
use std::process::Command;

fn token(expires_ms: i64) -> Credential {
    Credential::Oauth(OAuthCredential {
        access: "access".into(),
        refresh: "refresh".into(),
        expires_ms,
        extra: BTreeMap::new(),
    })
}

#[test]
fn expiry_conversion_is_explicit_and_never_extends_validity() {
    let legacy = to_legacy(&token(1_700_000_000_999)).unwrap();
    assert_eq!(legacy["expires"], 1_700_000_000_i64);
    assert!(from_legacy(Some(&legacy)).unwrap() == Some(token(1_700_000_000_000)));
    assert!(from_legacy(Some(
        &serde_json::json!({"type":"oauth","access":"a","refresh":"r","expires":i64::MAX})
    ))
    .is_err());
    assert!(from_legacy(Some(
        &serde_json::json!({"type":"oauth","access":"a","expires":"bad"})
    ))
    .is_err());
    assert!(from_legacy(Some(
        &serde_json::json!({"type":"api_key","key":"key","env":null})
    ))
    .unwrap()
    .is_some());
}

#[tokio::test]
async fn request_reads_resolve_key_references_without_rewriting_the_stored_value() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("auth.json");
    for (key, expected) in [
        ("$RPI_FIXTURE_AUTH_KEY", Some("resolved")),
        ("prefix-${RPI_FIXTURE_AUTH_KEY}", Some("prefix-resolved")),
        ("$$RPI_FIXTURE_AUTH_KEY", Some("$RPI_FIXTURE_AUTH_KEY")),
        ("$!echo literal", Some("!echo literal")),
        ("!echo resolved", Some("resolved")),
        ("$RPI_FIXTURE_AUTH_MISSING", None),
    ] {
        let bytes = serde_json::json!({"test":{"type":"api_key","key":key,"env":{"RPI_FIXTURE_AUTH_KEY":"resolved"}}}).to_string();
        std::fs::write(&path, &bytes).unwrap();
        let store = CompatibilityCredentialStore::new(path.clone()).unwrap();
        let Some(Credential::ApiKey { key: resolved, .. }) = store.read("test").await.unwrap()
        else {
            panic!("missing key credential")
        };
        assert_eq!(resolved.as_deref(), expected);
        let transaction = store.lock("test", CancellationToken::new()).await.unwrap();
        let Some(Credential::ApiKey { key: stored, .. }) = transaction.current() else {
            panic!("missing raw transaction credential")
        };
        assert_eq!(stored.as_deref(), Some(key));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn refresh_and_login_preserve_other_providers_and_extension_fields() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("auth.json");
    std::fs::write(&path, r#"{"other":{"type":"api_key","key":"other","futureField":"retain"},"test":{"type":"oauth","access":"old","refresh":"old","expires":1}}"#).unwrap();
    let store = CompatibilityCredentialStore::new(path.clone()).unwrap();
    let mut transaction = store.lock("test", CancellationToken::new()).await.unwrap();
    assert!(
        matches!(transaction.current(), Some(Credential::Oauth(credential)) if credential.expires_ms == 1000)
    );
    transaction.replace(token(2_000_000)).await.unwrap();
    drop(transaction);
    upsert(
        path.clone(),
        "new",
        &super::super::Credential::ApiKey {
            key: Some("key".into()),
            env: None,
        },
    )
    .unwrap();
    let values = read(&path).unwrap();
    assert_eq!(values["other"]["futureField"], "retain");
    assert_eq!(values["test"]["expires"], 2000);
    assert!(delete(path.clone(), "new").unwrap());
    assert!(!delete(path.clone(), "missing").unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn cancellation_releases_a_waiter_without_disturbing_lock_owner() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("auth.json");
    let store = CompatibilityCredentialStore::new(path.clone()).unwrap();
    let owner = store.lock("test", CancellationToken::new()).await.unwrap();
    let signal = CancellationToken::new();
    let waiting = tokio::spawn({
        let signal = signal.clone();
        async move { store.lock("test", signal).await }
    });
    signal.cancel();
    assert!(matches!(waiting.await.unwrap(), Err(AuthError::Cancelled)));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("json.lock"))
        .unwrap();
    assert!(lock.try_lock_exclusive().is_err());
    drop(owner);
    lock.try_lock_exclusive().unwrap();
}

#[test]
fn malformed_store_is_never_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("auth.json");
    std::fs::write(&path, b"{truncated").unwrap();
    let credential = super::super::Credential::ApiKey {
        key: Some("secret".into()),
        env: None,
    };
    assert!(upsert(path.clone(), "test", &credential).is_err());
    assert!(delete(path.clone(), "test").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"{truncated");
}

#[tokio::test]
async fn failed_publication_is_not_acknowledged_as_a_committed_credential() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("auth.json");
    std::fs::write(
        &path,
        r#"{"test":{"type":"oauth","access":"old","refresh":"old","expires":1}}"#,
    )
    .unwrap();
    let store = CompatibilityCredentialStore::new(path.clone()).unwrap();
    let mut transaction = store.lock("test", CancellationToken::new()).await.unwrap();
    let original = transaction.current();
    // A conflicting filesystem object makes rename fail on every platform.
    let saved = root.path().join("original.json");
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_eq!(
        transaction.replace(token(2_000_000)).await,
        Err(AuthError::Store)
    );
    assert!(transaction.current() == original);
    assert_eq!(read(&saved).unwrap()["test"]["refresh"], "old");
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(saved, &path).unwrap();
    transaction.replace(token(2_000_000)).await.unwrap();
    assert!(transaction.current() == Some(token(2_000_000)));
}

#[test]
fn relative_credential_paths_are_rejected() {
    assert!(CompatibilityCredentialStore::new(PathBuf::from("auth.json")).is_err());
}

#[test]
fn child_writer() {
    let Some(path) = std::env::var_os("RPI_TEST_AUTH_STORE") else {
        return;
    };
    let prefix = std::env::var("RPI_TEST_AUTH_PREFIX").unwrap();
    for index in 0..20 {
        upsert(
            PathBuf::from(&path),
            &format!("{prefix}-{index}"),
            &super::super::Credential::ApiKey {
                key: Some("test-only".into()),
                env: None,
            },
        )
        .unwrap();
    }
}

#[test]
fn competing_processes_do_not_lose_credential_updates() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("auth.json");
    let children: Vec<_> = (0..4)
        .map(|index| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "config::auth_store::tests::child_writer"])
                .env("RPI_TEST_AUTH_STORE", &path)
                .env("RPI_TEST_AUTH_PREFIX", index.to_string())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(read(&path).unwrap().len(), 80);
}

#[tokio::test]
async fn native_file_lease_shares_login_lock_and_does_not_parse_or_rewrite_private_bytes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("auth.json");
    let bytes = b"{deliberately malformed credential fixture";
    std::fs::write(&path, bytes).unwrap();
    let store = CompatibilityCredentialStore::new(path.clone()).unwrap();
    let lease = store
        .lock_file(CancellationToken::new(), std::time::Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(lease.path(), path);
    assert!(
        matches!(store.lock_file(CancellationToken::new(),std::time::Duration::ZERO).await,Err(AuthError::Provider(message)) if message == "credential store lock timed out")
    );
    let signal = CancellationToken::new();
    signal.cancel();
    assert!(matches!(
        store
            .lock_file(signal, std::time::Duration::from_secs(1))
            .await,
        Err(AuthError::Cancelled)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    drop(lease);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("json.lock"))
        .unwrap();
    lock.try_lock_exclusive().unwrap();
}
