use super::*;

impl Continuation {
    pub(super) fn target(&self, input: &Value) -> Result<ResolvedDelegationTarget, String> {
        let caller = self.caller.as_ref().ok_or("delegation v2 unavailable")?;
        self.capabilities.lock().unwrap().target(
            input["target"]
                .as_str()
                .ok_or("missing delegation target")?,
            caller,
        )
    }
    pub(super) async fn operate_v2(
        &self,
        input: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let caller = self.caller.as_ref().ok_or("delegation v2 unavailable")?;
        caller.validate()?;
        let binding = self.binding.as_ref().ok_or("delegation v2 unavailable")?;
        let resolver = binding.resolver()?;
        let epoch_signal = caller.epoch().signal();
        let work = async {
            match input["action"].as_str() {
                Some("resolveTarget") => {
                    let provider = input["provider"]
                        .as_str()
                        .ok_or("missing target provider")?;
                    let model = input["model"].as_str().ok_or("missing target model")?;
                    let target = resolver
                        .resolve_target(caller, provider, model, signal.clone())
                        .await?;
                    caller.validate()?;
                    if signal.is_cancelled() {
                        return Err("authentication cancelled".into());
                    }
                    let mut model = target.model().clone();
                    model.headers = None;
                    model.base_url.clear();
                    let id = self.capabilities.lock().unwrap().insert_target(target)?;
                    Ok(json!({"version":2,"target":id,"model":model}))
                }
                Some("auth.resolveAmbient") => {
                    let target = self.target(&input)?;
                    if input["provider"].as_str() != Some(target.model().provider.as_str())
                        || input["model"].as_str() != Some(target.model().id.as_str())
                    {
                        return Err("delegation target identity mismatch".into());
                    }
                    let auth = resolver
                        .resolve_ambient(caller, &target, signal.clone())
                        .await?;
                    target.validate(caller)?;
                    if signal.is_cancelled() {
                        return Err("authentication cancelled".into());
                    }
                    let id = self.capabilities.lock().unwrap().insert_auth(
                        input["target"].as_str().unwrap(),
                        caller,
                        ReceiptKind::Ambient,
                        auth.authentication(),
                    )?;
                    Ok(json!({"version":2,"receipt":id}))
                }
                Some("resolveAuth") => {
                    self.target(&input)?;
                    self.authenticate(input, signal.clone())
                        .await
                        .map_err(|error| error.to_string())
                }
                Some("releaseTarget") => {
                    self.capabilities.lock().unwrap().release_target(
                        input["target"]
                            .as_str()
                            .ok_or("missing delegation target")?,
                    )?;
                    Ok(json!({"version":2}))
                }
                Some("releaseAuth") => {
                    self.capabilities
                        .lock()
                        .unwrap()
                        .release_auth(input["receipt"].as_str().ok_or("missing auth receipt")?)?;
                    Ok(json!({"version":2}))
                }
                _ => Err("unknown continuation action".into()),
            }
        };
        tokio::select! { biased;
            _ = epoch_signal.cancelled() => Err("delegation epoch revoked".into()),
            _ = signal.cancelled() => Err("authentication cancelled".into()),
            result = work => result,
        }
    }
}
