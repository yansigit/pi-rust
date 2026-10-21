//! Explicit opt-in adapter for the existing second-based OAuth registration.

use super::*;

pub struct LegacyOAuthAuth {
    id: String,
    snapshot: Arc<RegistrySnapshot>,
    keepalive: Arc<PluginKeepalive>,
}

impl LegacyOAuthAuth {
    pub fn from_session(session: &ExtensionSession, id: &str) -> Option<Arc<Self>> {
        let snapshot = session.snapshot_arc()?;
        snapshot.oauth_provider(id)?;
        Some(Arc::new(Self {
            id: id.into(),
            snapshot,
            keepalive: session.keepalive(),
        }))
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
