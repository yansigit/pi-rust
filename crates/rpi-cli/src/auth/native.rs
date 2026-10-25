//! Native provider auth commands. Legacy registrations keep their old path.
use super::*;
use rpi_ai::auth::{
    AuthContext, AuthError, AuthOptions, Clock, Credential as NativeCredential, CredentialStore,
    SystemClock, MINIMUM_OAUTH_VALIDITY_MS,
};
use rpi_extensions::provider_auth::{
    login_with_pasted_key, AuthAction, AuthChoice, AuthInteraction, AuthMethod, AuthResponseKind,
    NativeProviderAuth,
};
use serde_json::{json, Value};
use std::io::IsTerminal;
use tokio_util::sync::CancellationToken;

pub(super) async fn run(
    args: &[String],
    session: &rpi_extensions::ExtensionSession,
    auth: Arc<NativeProviderAuth>,
) -> Option<i32> {
    let sub = args.first()?.as_str();
    if !matches!(
        sub,
        "login" | "refresh" | "logout" | "check" | "status" | "methods"
    ) {
        return None;
    }
    let signal = CancellationToken::new();
    let cancelled = signal.clone();
    let interrupt = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancelled.cancel();
        }
    });
    let result = execute(sub, args, session, auth, signal).await;
    interrupt.abort();
    Some(match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            EXIT_ERROR
        }
    })
}

async fn execute(
    sub: &str,
    args: &[String],
    session: &rpi_extensions::ExtensionSession,
    auth: Arc<NativeProviderAuth>,
    signal: CancellationToken,
) -> Result<i32, AuthError> {
    let provider = &auth.manifest().id;
    if sub == "methods" {
        println!(
            "{}",
            serde_json::to_string(&auth.methods(signal).await?).map_err(|_| AuthError::Resolve)?
        );
        return Ok(0);
    }
    let store = Arc::new(config::auth_store::CompatibilityCredentialStore::new(
        config::auth_path().map_err(|_| AuthError::Store)?,
    )?);
    match sub {
        "login" => {
            let methods = auth.methods(signal.clone()).await?;
            let choices = method_choices(&methods);
            let method = match args.iter().position(|arg| arg == "--method") {
                Some(index) => {
                    let requested = args.get(index + 1).ok_or(AuthError::Resolve)?;
                    methods
                        .iter()
                        .find(|method| &method.id == requested)
                        .ok_or(AuthError::Resolve)?
                }
                None => match choices.as_slice() {
                    [only] => methods
                        .iter()
                        .find(|method| method.id == only.id)
                        .ok_or(AuthError::Resolve)?,
                    [] => return Err(AuthError::Resolve),
                    _ => {
                        let id = select("Authentication method", choices, signal.clone()).await?;
                        methods
                            .iter()
                            .find(|method| method.id == id)
                            .ok_or(AuthError::Resolve)?
                    }
                },
            };
            login_with_method(&auth, method, store, signal.clone(), |interaction| {
                respond(interaction, signal.clone())
            })
            .await?;
            println!("Credentials saved for \"{provider}\".");
        }
        "refresh" => {
            if !matches!(
                store.read(provider).await?,
                Some(NativeCredential::Oauth(_))
            ) {
                return Err(AuthError::Refresh);
            }
            session
                .auth_runtime()
                .resolve(
                    provider.clone(),
                    auth.clone(),
                    store,
                    AuthContext::default(),
                    AuthOptions {
                        force_refresh: true,
                        ..Default::default()
                    },
                    signal,
                    Arc::new(SystemClock),
                )
                .await?
                .ok_or(AuthError::Resolve)?;
            println!("OAuth credentials refreshed for \"{provider}\".");
        }
        "logout" => {
            let mut transaction = store.lock(provider, signal.clone()).await?;
            if let Some(credential) = transaction.current() {
                let response = auth
                    .request(AuthAction::Revoke, json!({"credential":credential}), signal)
                    .await?;
                if response.kind != AuthResponseKind::Success {
                    return Err(AuthError::Resolve);
                }
                transaction.remove().await?;
            }
            println!("Credentials removed for \"{provider}\".");
        }
        "check" | "status" => {
            // A local probe must never refresh or contact a custody service.
            let stored = store.read(provider).await?;
            let (ready, source, refresh_required) = match stored {
                Some(NativeCredential::Oauth(credential)) if auth.manifest().oauth => {
                    let expired = credential.expires_ms
                        <= SystemClock
                            .now_ms()
                            .saturating_add(MINIMUM_OAUTH_VALIDITY_MS);
                    (!credential.access.is_empty() && !expired, "oauth", expired)
                }
                Some(NativeCredential::ApiKey { key, .. }) if auth.manifest().api_key => (
                    key.is_some_and(|key| !key.trim().is_empty()),
                    "auth.json",
                    false,
                ),
                Some(_) => (false, "unsupported", false),
                None => {
                    let config = config::load_models_config().map_err(|_| AuthError::Store)?;
                    match config
                        .providers
                        .get(provider)
                        .and_then(|config| config.api_key.as_deref())
                    {
                        Some(key) if auth.manifest().api_key => {
                            let env = std::env::vars_os()
                                .filter_map(|(key, value)| {
                                    Some((key.into_string().ok()?, value.into_string().ok()?))
                                })
                                .collect();
                            (
                                config::native_values::configured(key, &env),
                                "configured API key",
                                false,
                            )
                        }
                        _ => {
                            return Ok(super::run_check(
                                &args[1..],
                                session.snapshot_arc().as_ref(),
                            )
                            .await)
                        }
                    }
                }
            };
            if args.iter().any(|arg| arg == "--json") {
                println!(
                    "{}",
                    json!({"ready":ready,"provider":provider,"source":source,"refreshRequired":refresh_required})
                );
            } else {
                println!(
                    "{} ({source})",
                    if ready {
                        "ready"
                    } else if refresh_required {
                        "refresh_required"
                    } else {
                        "not_ready"
                    }
                );
            }
            return Ok(if ready { 0 } else { EXIT_NOT_READY });
        }
        _ => unreachable!(),
    }
    Ok(0)
}

async fn read_input(secret: bool, signal: CancellationToken) -> Result<String, AuthError> {
    let (sender, pending) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("rpi-auth-input".into())
        .spawn(move || {
            let result = (|| {
                if secret && io::stdin().is_terminal() {
                    return rpassword::read_password().map_err(|_| AuthError::Resolve);
                }
                let mut line = String::new();
                if io::stdin()
                    .read_line(&mut line)
                    .map_err(|_| AuthError::Resolve)?
                    == 0
                {
                    return Err(AuthError::Cancelled);
                }
                Ok(line.trim_end_matches(['\r', '\n']).to_string())
            })();
            let _ = sender.send(result);
        })
        .map_err(|_| AuthError::Task)?;
    tokio::select! {
        biased;
        _ = signal.cancelled() => Err(AuthError::Cancelled),
        result = pending => result.map_err(|_| AuthError::Task)?,
    }
}

/// The one row-label rule (`AuthMethod::display_label`), shared with the TUI so
/// the two surfaces cannot drift.
fn method_choices(methods: &[AuthMethod]) -> Vec<AuthChoice> {
    methods
        .iter()
        .map(|method| AuthChoice {
            id: method.id.clone(),
            label: method.display_label().into_owned(),
        })
        .collect()
}

/// Pi's `api_key_paste` route (`multiprovider.ts:1196-1208`): a method that only
/// discovers an ambient credential never enters the provider's login flow, so
/// the host prompts for the key itself and commits it.
async fn login_with_method<F, Fut>(
    auth: &NativeProviderAuth,
    method: &AuthMethod,
    store: Arc<dyn CredentialStore>,
    signal: CancellationToken,
    interact: F,
) -> Result<(), AuthError>
where
    F: FnMut(AuthInteraction) -> Fut,
    Fut: std::future::Future<Output = Result<Value, AuthError>>,
{
    if method.requires_paste_prompt() {
        return login_with_pasted_key(&auth.manifest().id, method, store, signal, interact).await;
    }
    auth.login(&method.id, store, signal, interact).await
}

async fn select(
    title: &str,
    choices: Vec<AuthChoice>,
    signal: CancellationToken,
) -> Result<String, AuthError> {
    println!("{title}");
    for (index, choice) in choices.iter().enumerate() {
        println!("{}. {}", index + 1, choice.label);
    }
    loop {
        eprint!("Selection: ");
        io::stderr().flush().map_err(|_| AuthError::Resolve)?;
        let input = read_input(false, signal.clone()).await?;
        if let Some(choice) = input
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| choices.get(index))
        {
            return Ok(choice.id.clone());
        }
        if let Some(choice) = choices.iter().find(|choice| choice.id == input) {
            return Ok(choice.id.clone());
        }
        eprintln!("Choose a listed number or method ID.");
    }
}

async fn respond(
    interaction: AuthInteraction,
    signal: CancellationToken,
) -> Result<Value, AuthError> {
    match interaction {
        AuthInteraction::Input {
            title,
            message,
            url,
            secret,
            allow_empty,
            continuation,
        } => {
            println!("{title}");
            if let Some(message) = message {
                println!("{message}");
            }
            if let Some(url) = url {
                println!("{url}");
            }
            let value = loop {
                eprint!("Input: ");
                io::stderr().flush().map_err(|_| AuthError::Resolve)?;
                let value = read_input(secret, signal.clone()).await?;
                if allow_empty || !value.is_empty() {
                    break value;
                }
                eprintln!("A value is required.");
            };
            Ok(json!({"value":value,"continuation":continuation}))
        }
        AuthInteraction::Select {
            title,
            choices,
            continuation,
        } => {
            let value = select(&title, choices, signal).await?;
            Ok(json!({"value":value,"continuation":continuation}))
        }
        AuthInteraction::Wait {
            message,
            poll_after_ms,
            continuation,
        } => {
            if let Some(message) = message {
                println!("{message}");
            }
            tokio::select! {
                biased;
                _ = signal.cancelled() => return Err(AuthError::Cancelled),
                _ = tokio::time::sleep(std::time::Duration::from_millis(poll_after_ms)) => {}
            }
            Ok(json!({"continuation":continuation}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_extensions::provider_auth::{AuthManifest, RegisteredAuthProvider};
    use rpi_extensions::{ExtensionRegistry, ExtensionSession, PluginKeepalive};
    use std::sync::Mutex;

    const MANIFEST: &str = r#"{"version":1,"id":"example","displayName":"Example","apiKey":true,"oauth":true,"methods":[{"id":"key","name":"Example API key","kind":"api_key","interactiveLogin":false},{"id":"browser","name":"Browser login","kind":"oauth","loginLabel":"Sign in with Example"}]}"#;

    #[derive(Default)]
    struct Fixture {
        requests: Mutex<Vec<Value>>,
    }

    extern "C" fn request(
        input: rpi_plugin_sdk::StbStringRef,
        _: *const rpi_plugin_sdk::AuthRequestControl,
        out: *mut rpi_plugin_sdk::StbString,
        context: *mut std::ffi::c_void,
    ) -> i32 {
        rpi_plugin_sdk::guard_or(70, || {
            // SAFETY: the fixture outlives the session built over it.
            let fixture = unsafe { &*context.cast::<Fixture>() };
            let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
            fixture.requests.lock().unwrap().push(input);
            let response = json!({
                "version":1,
                "kind":"credential",
                "payload":{"type":"oauth","access":"login-access","refresh":"login-refresh","expires":1234567890123_i64}
            });
            unsafe {
                *out = rpi_plugin_sdk::StbString::from_string(response.to_string());
            }
            0
        })
    }

    fn session(fixture: &Arc<Fixture>) -> ExtensionSession {
        let mut registry = ExtensionRegistry::new();
        assert!(!registry.register_provider_auth(RegisteredAuthProvider {
            action_bridge: None,
            plugin: "fixture".into(),
            manifest: AuthManifest::parse(MANIFEST).unwrap(),
            request_fn: request,
            plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
            user_data: Arc::as_ptr(fixture).cast_mut().cast(),
        }));
        ExtensionSession::from_parts(
            Arc::new(registry.snapshot()),
            PluginKeepalive::empty(),
            vec![],
            None,
        )
    }

    struct MemoryStore {
        credential: Mutex<Option<NativeCredential>>,
    }
    struct MemoryTransaction(Arc<MemoryStore>);
    struct MemoryStoreHandle(Arc<MemoryStore>);

    #[async_trait::async_trait]
    impl rpi_ai::auth::CredentialTransaction for MemoryTransaction {
        fn current(&self) -> Option<NativeCredential> {
            self.0.credential.lock().unwrap().clone()
        }
        async fn replace(&mut self, credential: NativeCredential) -> Result<(), AuthError> {
            *self.0.credential.lock().unwrap() = Some(credential);
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl CredentialStore for MemoryStoreHandle {
        async fn read(&self, _: &str) -> Result<Option<NativeCredential>, AuthError> {
            Ok(self.0.credential.lock().unwrap().clone())
        }
        async fn lock(
            &self,
            _: &str,
            _: CancellationToken,
        ) -> Result<Box<dyn rpi_ai::auth::CredentialTransaction>, AuthError> {
            Ok(Box::new(MemoryTransaction(self.0.clone())))
        }
    }

    fn memory_store() -> (Arc<MemoryStore>, Arc<dyn CredentialStore>) {
        let store = Arc::new(MemoryStore {
            credential: Mutex::new(None),
        });
        let handle: Arc<dyn CredentialStore> = Arc::new(MemoryStoreHandle(store.clone()));
        (store, handle)
    }

    #[tokio::test]
    async fn login_rows_and_paste_routing_follow_pis_auth_choices() {
        let fixture = Arc::new(Fixture::default());
        let session = session(&fixture);
        let auth = NativeProviderAuth::from_session(&session, "example").unwrap();
        let methods = auth.methods(CancellationToken::new()).await.unwrap();
        assert_eq!(
            method_choices(&methods)
                .iter()
                .map(|choice| (choice.id.as_str(), choice.label.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("key", "Example API key (paste)"),
                ("browser", "Sign in with Example"),
            ]
        );

        // `api_key_paste` asks the host for the key and never calls the provider.
        let (store, handle) = memory_store();
        let mut prompts = Vec::new();
        login_with_method(
            &auth,
            &methods[0],
            handle,
            CancellationToken::new(),
            |interaction| {
                prompts.push(serde_json::to_value(&interaction).unwrap());
                async { Ok(json!({"value":"typed-key"})) }
            },
        )
        .await
        .unwrap();
        assert_eq!(
            prompts,
            vec![json!({
                "kind":"input","title":"Enter Example API key:",
                "message":null,"url":null,"secret":true,
                "allowEmpty":false,"continuation":null
            })]
        );
        assert!(matches!(
            store.credential.lock().unwrap().clone(),
            Some(NativeCredential::ApiKey { key: Some(key), .. }) if key == "typed-key"
        ));
        assert!(fixture.requests.lock().unwrap().is_empty());

        // An interactive OAuth method still enters the provider's own flow, and
        // only its acknowledged credential commit is stored.
        let (store, handle) = memory_store();
        login_with_method(
            &auth,
            &methods[1],
            handle,
            CancellationToken::new(),
            |_| async { Err::<Value, AuthError>(AuthError::Resolve) },
        )
        .await
        .unwrap();
        {
            let requests = fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["action"], "login");
            assert_eq!(requests[0]["input"]["methodId"], "browser");
        }
        assert!(matches!(
            store.credential.lock().unwrap().clone(),
            Some(NativeCredential::Oauth(_))
        ));
    }
}
