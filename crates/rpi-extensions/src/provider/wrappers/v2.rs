use super::*;

/// v2 credentials come only from the selected target account or host ambient
/// auth. An empty env is accepted for SDK compatibility; no caller overrides
/// (including apparently non-secret ones) have trusted target provenance.
pub(super) fn account_auth_options(input: &Value) -> Result<AuthOptions, String> {
    #[derive(Default, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields, default)]
    struct Options {
        env: std::collections::BTreeMap<String, String>,
        minimum_validity_ms: Option<i64>,
    }
    let options: Options = if input.is_null() {
        Options::default()
    } else {
        serde_json::from_value(input.clone()).map_err(|_| {
            "v2 auth options permit only minimumValidityMs and empty env".to_string()
        })?
    };
    if !options.env.is_empty() {
        return Err("v2 auth options forbid caller environment overrides".into());
    }
    Ok(AuthOptions {
        minimum_validity_ms: options.minimum_validity_ms,
        ..Default::default()
    })
}

impl Continuation {
    pub(super) fn target(&self, input: &Value) -> Result<ResolvedDelegationTarget, String> {
        let caller = self.caller.as_ref().ok_or("delegation v2 unavailable")?;
        self.admission()?.tables.target(
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
                    let id = self.admission()?.tables.insert_target(target)?;
                    Ok(json!({"version":2,"target":id,"model":model}))
                }
                Some("auth.resolveAmbient") => {
                    account_auth_options(&input["options"])?;
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
                    let id = self.admission()?.tables.insert_auth(
                        input["target"].as_str().unwrap(),
                        caller,
                        ReceiptKind::Ambient,
                        auth.authentication(),
                    )?;
                    Ok(json!({"version":2,"receipt":id}))
                }
                Some("resolveAuth") => {
                    account_auth_options(&input["options"])?;
                    let target = self.target(&input)?;
                    let released = target.signal();
                    tokio::select! { biased;
                        _ = released.cancelled() => Err("delegation target released".into()),
                        result = self.authenticate(input, signal.clone()) => result.map_err(|error| error.to_string()),
                    }
                }
                Some("releaseTarget") => {
                    self.admission()?.tables.release_target(
                        input["target"]
                            .as_str()
                            .ok_or("missing delegation target")?,
                    )?;
                    Ok(json!({"version":2}))
                }
                Some("releaseAuth") => {
                    self.admission()?
                        .tables
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
