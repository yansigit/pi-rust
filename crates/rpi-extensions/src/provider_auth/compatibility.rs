//! Auth used by the explicit native management/pooling path. Legacy ambient
//! request resolution stays unchanged until a provider opts into native auth.
use super::*;

pub enum ProviderAuthentication {
    Native(Arc<NativeProviderAuth>),
    Legacy(Arc<LegacyOAuthAuth>),
    ApiKey(ApiKeyAuthentication),
}

impl ProviderAuthentication {
    /// The caller must have resolved `id` to an actual registered provider.
    /// Protocol providers without a custom auth flow use the host's key prompt.
    pub fn for_registered_provider(session: &ExtensionSession, id: &str) -> Arc<Self> {
        Arc::new(
            if let Some(auth) = NativeProviderAuth::from_session(session, id) {
                Self::Native(auth)
            } else if let Some(auth) = LegacyOAuthAuth::from_session(session, id) {
                Self::Legacy(auth)
            } else {
                Self::ApiKey(ApiKeyAuthentication(AuthManifest {
                    version: 1,
                    id: id.into(),
                    display_name: id.into(),
                    api_key: true,
                    oauth: false,
                    methods: vec![AuthMethod {
                        id: "api_key".into(),
                        name: "API key".into(),
                        kind: AuthMethodKind::ApiKey,
                    }],
                }))
            },
        )
    }

    pub fn manifest(&self) -> &AuthManifest {
        match self {
            Self::Native(auth) => auth.manifest(),
            Self::Legacy(auth) => &auth.manifest,
            Self::ApiKey(auth) => &auth.0,
        }
    }

    fn resolver(&self) -> &dyn ProviderAuth {
        match self {
            Self::Native(auth) => auth.as_ref(),
            Self::Legacy(auth) => auth.as_ref(),
            Self::ApiKey(auth) => auth,
        }
    }

    pub async fn request(
        &self,
        action: AuthAction,
        input: Value,
        signal: CancellationToken,
    ) -> Result<AuthResponse, AuthError> {
        if signal.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        match self {
            Self::Native(auth) => auth.request(action, input, signal).await,
            Self::Legacy(auth) => auth.request(action, input, signal).await,
            Self::ApiKey(_) => {
                let (kind, payload) = match action {
                    AuthAction::Login if input["methodId"] == "api_key" => (
                        AuthResponseKind::Interaction,
                        json!({"kind":"input","title":"API key","secret":true,"continuation":null}),
                    ),
                    AuthAction::Continue => {
                        let key = input["value"]
                            .as_str()
                            .filter(|key| !key.trim().is_empty())
                            .ok_or(AuthError::Resolve)?;
                        (
                            AuthResponseKind::Credential,
                            json!({"type":"api_key","key":key}),
                        )
                    }
                    AuthAction::Revoke => (AuthResponseKind::Success, Value::Null),
                    _ => return Err(AuthError::Resolve),
                };
                Ok(AuthResponse {
                    version: 1,
                    kind,
                    payload,
                })
            }
        }
    }
}

#[async_trait::async_trait]
impl ProviderAuth for ProviderAuthentication {
    fn supports_api_key(&self) -> bool {
        self.resolver().supports_api_key()
    }
    fn supports_oauth(&self) -> bool {
        self.resolver().supports_oauth()
    }
    async fn resolve_api_key(
        &self,
        context: AuthContext,
        credential: Option<Credential>,
        signal: CancellationToken,
    ) -> Result<Option<RequestAuth>, AuthError> {
        self.resolver()
            .resolve_api_key(context, credential, signal)
            .await
    }
    async fn refresh(
        &self,
        credential: OAuthCredential,
        signal: CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        self.resolver().refresh(credential, signal).await
    }
    async fn to_auth(&self, credential: OAuthCredential) -> Result<RequestAuth, AuthError> {
        self.resolver().to_auth(credential).await
    }
}

pub struct ApiKeyAuthentication(AuthManifest);
#[async_trait::async_trait]
impl ProviderAuth for ApiKeyAuthentication {
    fn supports_api_key(&self) -> bool {
        true
    }
    fn supports_oauth(&self) -> bool {
        false
    }
    async fn resolve_api_key(
        &self,
        _: AuthContext,
        credential: Option<Credential>,
        signal: CancellationToken,
    ) -> Result<Option<RequestAuth>, AuthError> {
        if signal.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        Ok(match credential {
            Some(Credential::ApiKey {
                key: Some(key),
                env,
            }) if !key.trim().is_empty() => Some(RequestAuth {
                api_key: Some(key),
                env,
                ..Default::default()
            }),
            _ => None,
        })
    }
    async fn refresh(
        &self,
        _: OAuthCredential,
        _: CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        Err(AuthError::Refresh)
    }
    async fn to_auth(&self, _: OAuthCredential) -> Result<RequestAuth, AuthError> {
        Err(AuthError::Resolve)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn host_key_flow_and_resolution_do_not_read_ambient_credentials() {
        let session = ExtensionSession::from_parts(
            Arc::new(crate::ExtensionRegistry::new().snapshot()),
            PluginKeepalive::empty(),
            vec![],
            None,
        );
        let auth = ProviderAuthentication::for_registered_provider(&session, "protocol");
        let signal = CancellationToken::new();
        let response = auth
            .request(
                AuthAction::Login,
                json!({"methodId":"api_key"}),
                signal.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(
            AuthInteraction::parse(response.payload).unwrap(),
            AuthInteraction::Input { secret: true, .. }
        ));
        assert!(auth
            .request(AuthAction::Continue, json!({"value":"  "}), signal.clone())
            .await
            .is_err());
        let response = auth
            .request(
                AuthAction::Continue,
                json!({"value":"private-key"}),
                signal.clone(),
            )
            .await
            .unwrap();
        assert!(response.kind == AuthResponseKind::Credential);
        let mut credential = response.payload;
        credential["env"] = json!({"REGION":"pool-region"});
        let credential = serde_json::from_value(credential).unwrap();
        let context = AuthContext {
            env: [("PROTOCOL_API_KEY".into(), "ambient-secret".into())].into(),
        };
        assert!(auth
            .resolve_api_key(context.clone(), None, signal.clone())
            .await
            .unwrap()
            .is_none());
        let resolved = auth
            .resolve_api_key(context, Some(credential), signal.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.api_key.as_deref(), Some("private-key"));
        assert_eq!(resolved.env["REGION"], "pool-region");
        assert!(resolved.headers.is_empty());
        signal.cancel();
        assert!(matches!(
            auth.request(AuthAction::Revoke, Value::Null, signal).await,
            Err(AuthError::Cancelled)
        ));
    }
}
