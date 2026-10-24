use super::*;
fn continuation() -> Continuation {
    super::tests::continuation()
}
extern "C" fn old(
    _: StbStringRef,
    _: *const crate::AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    unsafe {
        *out = StbString::from_string(json!({"error":"unknown continuation action"}).to_string());
    }
    1
}
extern "C" fn modern(
    input: StbStringRef,
    _: *const crate::AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
    let value = match input["action"].as_str().unwrap() {
        "capabilities" => {
            json!({"version":1,"delegationVersions":[1,2],"opaqueTargets":true,"ambientReceipts":true,"epochRevocation":true})
        }
        "resolveTarget" => {
            json!({"version":2,"target":"opaque","model":Model::new("m","M",rpi_ai::Api::Faux,"b","")})
        }
        "auth.resolveAmbient" | "resolveAuth" => {
            assert_eq!(input["target"], "opaque");
            json!({"version":2,"receipt":"sealed"})
        }
        "releaseTarget" | "releaseAuth" => json!({"version":2}),
        _ => panic!("unexpected SDK operation"),
    };
    unsafe {
        *out = StbString::from_string(value.to_string());
    }
    0
}
extern "C" fn delegated(
    input: StbStringRef,
    _: Option<ProviderStreamEmitFn>,
    _: *mut c_void,
    _: *const ProviderStreamControl,
    _: Option<crate::ProviderStreamCompletionFn>,
    _: *mut c_void,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
    assert_eq!(input["version"], 2);
    assert_eq!(input["target"], "opaque");
    assert_eq!(input["authReceipt"], "sealed");
    assert!(input.get("model").is_none());
    for key in ["apiKey", "headers", "env", "authMode", "identity"] {
        assert!(input["options"].get(key).is_none());
    }
    let mut message = AssistantMessage::empty(rpi_ai::Api::Faux, "b", "m", 0);
    message.stop_reason = StopReason::Stop;
    unsafe {
        *out = StbString::from_string(serde_json::to_string(&message).unwrap());
    }
    0
}
#[tokio::test]
async fn old_or_short_host_refuses_v2_without_downgrade() {
    let mut next = continuation();
    assert!(!next.supports_delegation_v2().await.unwrap());
    next.api.provider_continuation = Some(old);
    assert!(!next.supports_delegation_v2().await.unwrap());
    assert_eq!(
        next.resolve_target("b", "m", CancellationToken::new())
            .await
            .err()
            .unwrap(),
        "host lacks secure delegation v2"
    );
}
#[tokio::test]
async fn opaque_sdk_envelopes_and_owner_checks() {
    let mut next = continuation();
    next.api.provider_continuation = Some(modern);
    next.api.delegate_provider_stream = Some(delegated);
    assert!(next.supports_delegation_v2().await.unwrap());
    let target = next
        .resolve_target("b", "m", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(target.model().provider, "b");
    let receipt = next
        .resolve_ambient(&target, CancellationToken::new())
        .await
        .unwrap();
    let mut foreign = next.clone();
    foreign.descriptor["id"] = json!("foreign");
    assert_eq!(
        foreign.delegate(&target, None).err().unwrap(),
        "delegation target belongs to a different continuation"
    );
    assert_eq!(
        foreign.release_auth(receipt.clone()).await.unwrap_err(),
        "auth receipt belongs to a different continuation"
    );
    let mut wrong_target = target.clone();
    wrong_target.handle = "different".into();
    assert_eq!(
        next.delegate(&wrong_target, Some(receipt.clone()))
            .err()
            .unwrap(),
        "auth receipt target mismatch"
    );
    let provider = next.delegate(&target, Some(receipt.clone())).unwrap();
    assert_eq!(provider.id(), "b");
    let mut options = SimpleStreamOptions::default();
    options.api_key = Some("must-not-cross".into());
    let message = provider
        .stream_simple(target.model(), &Context::default(), &options)
        .await
        .result()
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Stop);
    next.release_auth(receipt).await.unwrap();
    let account = next
        .resolve_target_auth(&target, "store", "account", &options)
        .await
        .unwrap();
    next.release_auth(account).await.unwrap();
    next.release_target(target).await.unwrap();
}
