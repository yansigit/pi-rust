use super::*;

/// Sanitized metadata plus a continuation-owned, opaque host capability.
/// Deliberately neither Debug nor Serialize.
#[derive(Clone)]
pub struct AmbientTarget {
    pub(super) owner: Value,
    pub(super) handle: String,
    pub(super) model: Model,
}
impl AmbientTarget {
    pub fn model(&self) -> &Model {
        &self.model
    }
}
impl Continuation {
    pub async fn supports_delegation_v2(&self) -> Result<bool, String> {
        if self.api.provider_continuation.is_none() {
            return Ok(false);
        }
        match self
            .operation(
                json!({"version":1,"action":"capabilities"}),
                CancellationToken::new(),
            )
            .await
        {
            Ok(value) => Ok(value["version"] == 1
                && value["delegationVersions"]
                    .as_array()
                    .is_some_and(|versions| versions.contains(&json!(2)))
                && value["opaqueTargets"] == true
                && value["ambientReceipts"] == true
                && value["epochRevocation"] == true),
            // Pre-v2 hosts reject the unknown action. This is not permission to
            // downgrade a requested cross-provider call to v1.
            Err(_) => Ok(false),
        }
    }
    pub async fn resolve_target(
        &self,
        provider: &str,
        model: &str,
        signal: CancellationToken,
    ) -> Result<AmbientTarget, String> {
        if !self.supports_delegation_v2().await? {
            return Err("host lacks secure delegation v2".into());
        }
        let value = self
            .operation(
                json!({"version":2,"action":"resolveTarget","provider":provider,"model":model}),
                signal,
            )
            .await?;
        if value["version"] != 2 {
            return Err("unsupported delegation target".into());
        }
        let metadata: Model = serde_json::from_value(value["model"].clone())
            .map_err(|_| "invalid target metadata")?;
        if metadata.provider != provider || metadata.id != model {
            return Err("target metadata identity mismatch".into());
        }
        Ok(AmbientTarget {
            owner: self.descriptor.clone(),
            handle: value["target"]
                .as_str()
                .ok_or("missing delegation target")?
                .into(),
            model: metadata,
        })
    }
    pub async fn resolve_ambient(
        &self,
        target: &AmbientTarget,
        signal: CancellationToken,
    ) -> Result<AuthReceipt, String> {
        self.owns(target)?;
        let value = self.operation(json!({"version":2,"action":"auth.resolveAmbient","provider":target.model.provider,"model":target.model.id,"target":target.handle}), signal).await?;
        self.target_receipt(target, value)
    }
    pub async fn resolve_target_auth(
        &self,
        target: &AmbientTarget,
        store_id: &str,
        account_id: &str,
        options: &SimpleStreamOptions,
    ) -> Result<AuthReceipt, String> {
        self.owns(target)?;
        let value = self.operation(json!({"version":2,"action":"resolveAuth","target":target.handle,"storeId":store_id,"accountId":account_id,"options":{"env":options.env}}), options.signal.clone()).await?;
        self.target_receipt(target, value)
    }
    pub fn delegate(
        &self,
        target: &AmbientTarget,
        receipt: Option<AuthReceipt>,
    ) -> Result<Arc<dyn Provider>, String> {
        self.owns(target)?;
        if receipt.as_ref().is_some_and(|receipt| {
            receipt.owner != self.descriptor
                || receipt.target.as_deref() != Some(target.handle.as_str())
                || receipt.version != 2
        }) {
            return Err("auth receipt target mismatch".into());
        }
        Ok(Arc::new(DelegatedProvider {
            continuation: self.clone(),
            receipt,
            models: vec![target.model.clone()],
            target: Some(target.clone()),
        }))
    }
    pub async fn release_target(&self, target: AmbientTarget) -> Result<(), String> {
        self.owns(&target)?;
        self.operation(
            json!({"version":2,"action":"releaseTarget","target":target.handle}),
            CancellationToken::new(),
        )
        .await
        .map(|_| ())
    }
    fn owns(&self, target: &AmbientTarget) -> Result<(), String> {
        if target.owner != self.descriptor {
            Err("delegation target belongs to a different continuation".into())
        } else {
            Ok(())
        }
    }
    fn target_receipt(&self, target: &AmbientTarget, value: Value) -> Result<AuthReceipt, String> {
        if value["version"] != 2 {
            return Err("unsupported auth receipt".into());
        }
        Ok(AuthReceipt {
            id: value["receipt"]
                .as_str()
                .ok_or("missing auth receipt")?
                .into(),
            owner: self.descriptor.clone(),
            target: Some(target.handle.clone()),
            version: 2,
        })
    }
    pub(super) async fn operation(
        &self,
        mut request: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let callback = self
            .api
            .provider_continuation
            .ok_or("host lacks private provider continuation operations")?;
        request["continuation"] = self.descriptor.clone();
        let signal = signal.child_token();
        let _cancel = signal.clone().drop_guard();
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            if signal.is_cancelled() {
                return Err("provider operation was cancelled".into());
            }
            let options = SimpleStreamOptions::default().with_signal(signal);
            let control = crate::AuthRequestControl {
                struct_size: std::mem::size_of::<crate::AuthRequestControl>() as u32,
                is_cancelled: auth_cancelled,
                user_data: (&options as *const SimpleStreamOptions).cast_mut().cast(),
            };
            let mut out = StbString::empty();
            let status = callback(
                StbStringRef::from_str(&request.to_string()),
                &control,
                &mut out,
                this.api.user_data,
            );
            let result = host_result(&this.api, out, status);
            if options.signal.is_cancelled() {
                Err("provider operation was cancelled".into())
            } else {
                result
            }
        })
        .await
        .map_err(|_| "provider operation task stopped".to_owned())?
    }
}
