//! Explicit opt-in adapter for the existing second-based OAuth registration.

use super::*;

pub struct LegacyOAuthAuth {
    id: String,
    pub(super) manifest: AuthManifest,
    snapshot: Arc<RegistrySnapshot>,
    keepalive: Arc<PluginKeepalive>,
}

impl LegacyOAuthAuth {
    pub fn from_session(session: &ExtensionSession, id: &str) -> Option<Arc<Self>> {
        let snapshot = session.snapshot_arc()?;
        let record = snapshot.oauth_provider(id)?;
        let value: Value = serde_json::from_str(&record.manifest_json).ok()?;
        Some(Arc::new(Self {
            id: id.into(),
            manifest: AuthManifest {
                version: 1,
                id: id.into(),
                display_name: value["displayName"].as_str().unwrap_or(id).into(),
                api_key: false,
                oauth: true,
                methods: vec![AuthMethod {
                    id: "oauth".into(),
                    name: "OAuth".into(),
                    kind: AuthMethodKind::Oauth,
                }],
            },
            snapshot,
            keepalive: session.keepalive(),
        }))
    }

    pub(super) async fn request(
        &self,
        action: AuthAction,
        input: Value,
        signal: CancellationToken,
    ) -> Result<AuthResponse, AuthError> {
        if !self.snapshot.is_active() || signal.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let (action, input) = match action {
            AuthAction::Login if input["methodId"] == "oauth" => ("begin", json!({})),
            AuthAction::Continue => ("exchange", json!({"value":input["value"]})),
            AuthAction::Revoke => {
                let Credential::Oauth(token) = serde_json::from_value(input["credential"].clone())
                    .map_err(|_| AuthError::Resolve)?
                else {
                    return Err(AuthError::Resolve);
                };
                (
                    "revoke",
                    json!({"credential":{"access":token.access,"refresh":token.refresh,"expires_at":token.expires_ms.div_euclid(1000)}}),
                )
            }
            _ => return Err(AuthError::Resolve),
        };
        let id = self.id.clone();
        let snapshot = self.snapshot.clone();
        let keepalive = self.keepalive.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _keepalive = keepalive;
            let response = crate::oauth::request(&snapshot, &id, action, input)
                .map_err(|_| AuthError::Resolve)?;
            crate::oauth::OAuthActionResponse::parse(response).map_err(|_| AuthError::Resolve)
        });
        let response = tokio::select! { biased; _ = signal.cancelled() => return Err(AuthError::Cancelled), result = task => result.map_err(|_| AuthError::Task)?? };
        let (kind, payload) = match response.kind {
            crate::oauth::OAuthResponseKind::Credential => {
                let token = response
                    .credential()
                    .map_err(|_| AuthError::Resolve)?
                    .ok_or(AuthError::Resolve)?;
                let token = Credential::Oauth(OAuthCredential {
                    access: token.access,
                    refresh: token.refresh,
                    expires_ms: token
                        .expires_at
                        .checked_mul(1000)
                        .ok_or(AuthError::Resolve)?,
                    extra: Default::default(),
                });
                (
                    AuthResponseKind::Credential,
                    serde_json::to_value(token).map_err(|_| AuthError::Resolve)?,
                )
            }
            crate::oauth::OAuthResponseKind::Interaction => {
                let display = response.payload.get("display").unwrap_or(&response.payload);
                let instructions = display["instructions"].as_array().map(|lines| {
                    lines
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                });
                (
                    AuthResponseKind::Interaction,
                    serde_json::to_value(AuthInteraction::Input {
                        title: display["title"].as_str().unwrap_or("OAuth input").into(),
                        message: instructions,
                        url: display["url"].as_str().map(str::to_owned),
                        secret: false,
                        allow_empty: false,
                        continuation: Value::Null,
                    })
                    .map_err(|_| AuthError::Resolve)?,
                )
            }
            crate::oauth::OAuthResponseKind::Success => (AuthResponseKind::Success, Value::Null),
            crate::oauth::OAuthResponseKind::Error => return Err(AuthError::Resolve),
        };
        Ok(AuthResponse {
            version: 1,
            kind,
            payload,
        })
    }
}

#[async_trait::async_trait]
impl ProviderAuth for LegacyOAuthAuth {
    fn supports_api_key(&self) -> bool {
        false
    }
    fn supports_oauth(&self) -> bool {
        true
    }

    async fn resolve_api_key(
        &self,
        _: AuthContext,
        _: Option<Credential>,
        _: CancellationToken,
    ) -> Result<Option<RequestAuth>, AuthError> {
        Ok(None)
    }

    async fn refresh(
        &self,
        credential: OAuthCredential,
        signal: CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        if !self.snapshot.is_active() || signal.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let id = self.id.clone();
        let snapshot = self.snapshot.clone();
        let keepalive = self.keepalive.clone();
        // Legacy callbacks have no cancellation control. Retain their context
        // until they return even if the compatibility deadline stops waiting.
        tokio::task::spawn_blocking(move || {
            let _keepalive = keepalive;
            let response = crate::oauth::request(&snapshot, &id, "refresh", json!({"credential":{
                "access":credential.access,"refresh":credential.refresh,"expires_at":credential.expires_ms.div_euclid(1000)
            }})).map_err(|_| AuthError::Refresh)?;
            let response = crate::oauth::OAuthActionResponse::parse(response).map_err(|_| AuthError::Refresh)?;
            let token = response.credential().map_err(|_| AuthError::Refresh)?.ok_or(AuthError::Refresh)?;
            Ok(OAuthCredential {
                access:token.access, refresh:token.refresh,
                expires_ms:token.expires_at.checked_mul(1000).ok_or(AuthError::Refresh)?,
                extra:credential.extra,
            })
        }).await.map_err(|_| AuthError::Task)?
    }

    async fn to_auth(&self, credential: OAuthCredential) -> Result<RequestAuth, AuthError> {
        Ok(RequestAuth {
            headers: [(
                "Authorization".into(),
                format!("Bearer {}", credential.access),
            )]
            .into(),
            api_key: Some(credential.access),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    rpi_plugin_sdk::string_handler! { fn callback(request, out, _context) {
        let input: Value = serde_json::from_str(request).unwrap();
        assert_eq!(input["input"]["credential"]["expires_at"], 1234);
        i32::from(!out.write(json!({"version":1,"kind":"credential","payload":{
            "access":"rotated","refresh":"rotated-refresh","expires_at":9876
        }}).to_string()))
    } }

    rpi_plugin_sdk::string_handler! { fn login_callback(request, out, _context) {
        let input: Value = serde_json::from_str(request).unwrap();
        let response = match input["action"].as_str().unwrap() {
            "begin" => json!({"version":1,"kind":"interaction","payload":{"display":{"title":"Connect account","url":"https://example.invalid/login","instructions":["Open URL","Paste code"]}}}),
            "exchange" => {
                assert_eq!(input["input"], json!({"value":"code"}));
                json!({"version":1,"kind":"credential","payload":{"access":"private-access","refresh":"private-refresh","expires_at":1234}})
            },
            "revoke" => {
                assert_eq!(input["input"]["credential"]["expires_at"], 1234);
                json!({"version":1,"kind":"success","payload":null})
            },
            _ => panic!("unexpected legacy action"),
        };
        i32::from(!out.write(response.to_string()))
    } }

    #[tokio::test]
    async fn native_management_adapts_legacy_login_and_logout_without_changing_expiry_units() {
        let mut registry = crate::ExtensionRegistry::new();
        registry.register_oauth_provider(crate::RegisteredOAuthProvider {
            plugin: "legacy".into(),
            manifest_json: r#"{"id":"legacy","displayName":"Existing provider"}"#.into(),
            request_fn: login_callback,
            plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
            user_data: std::ptr::null_mut(),
        });
        let session = ExtensionSession::from_parts(
            Arc::new(registry.snapshot()),
            PluginKeepalive::empty(),
            vec![],
            None,
        );
        let adapter = ProviderAuthentication::for_registered_provider(&session, "legacy");
        assert!(!adapter.supports_api_key());
        assert_eq!(adapter.manifest().display_name, "Existing provider");
        let response = adapter
            .request(
                AuthAction::Login,
                json!({"methodId":"oauth"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let AuthInteraction::Input {
            message,
            title,
            url,
            ..
        } = AuthInteraction::parse(response.payload).unwrap()
        else {
            panic!()
        };
        assert_eq!(title, "Connect account");
        assert_eq!(message.as_deref(), Some("Open URL\nPaste code"));
        assert_eq!(url.as_deref(), Some("https://example.invalid/login"));
        let response = adapter
            .request(
                AuthAction::Continue,
                json!({"value":"code","continuation":null}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.payload["expires"], 1234000);
        let response = adapter
            .request(
                AuthAction::Revoke,
                json!({"credential":response.payload}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(response.kind == AuthResponseKind::Success);
    }

    #[tokio::test]
    async fn old_registration_converts_seconds_only_at_its_boundary() {
        let mut registry = crate::ExtensionRegistry::new();
        registry.register_oauth_provider(crate::RegisteredOAuthProvider {
            plugin: "legacy".into(),
            manifest_json: r#"{"id":"legacy"}"#.into(),
            request_fn: callback,
            plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
            user_data: std::ptr::null_mut(),
        });
        let session = ExtensionSession::from_parts(
            Arc::new(registry.snapshot()),
            PluginKeepalive::empty(),
            vec![],
            None,
        );
        let adapter = LegacyOAuthAuth::from_session(&session, "legacy").unwrap();
        let result = adapter
            .refresh(
                OAuthCredential {
                    access: "a".into(),
                    refresh: "r".into(),
                    expires_ms: 1234567,
                    extra: Default::default(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.expires_ms, 9876000);
        let auth = adapter.to_auth(result).await.unwrap();
        assert_eq!(auth.headers["Authorization"], "Bearer rotated");
        assert!(!adapter.supports_api_key());
    }
}
