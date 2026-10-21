//! Native provider auth commands. Legacy registrations keep their old path.
use super::*;
use rpi_ai::auth::{
    AuthContext, AuthError, AuthOptions, Clock, Credential as NativeCredential, CredentialStore,
    SystemClock, MINIMUM_OAUTH_VALIDITY_MS,
};
use rpi_extensions::provider_auth::{
    AuthAction, AuthChoice, AuthInteraction, AuthResponseKind, NativeProviderAuth,
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
            let choices: Vec<_> = auth
                .methods(signal.clone())
                .await?
                .iter()
                .map(|method| AuthChoice {
                    id: method.id.clone(),
                    label: method.name.clone(),
                })
                .collect();
            let method = match args.iter().position(|arg| arg == "--method") {
                Some(index) => args.get(index + 1).cloned().ok_or(AuthError::Resolve)?,
                None => match choices.as_slice() {
                    [only] => only.id.clone(),
                    [] => return Err(AuthError::Resolve),
                    _ => select("Authentication method", choices, signal.clone()).await?,
                },
            };
            auth.login(&method, store, signal.clone(), |interaction| {
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
