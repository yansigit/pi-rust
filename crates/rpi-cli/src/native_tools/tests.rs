use super::*;
use crate::extensions_actions::HarnessActionHost;
use rpi_extensions::{native::NativeOwner, RuntimeActionHost};
use rpi_harness::{
    agent_harness::AgentHarness,
    types::{AgentHarnessOptions, AgentHarnessResources, ToolRegistrationPolicy},
};
use std::collections::BTreeMap;

fn owner() -> NativeOwner {
    NativeOwner {
        generation: "fixture-generation".into(),
        id: 12,
        plugin: "fixture".into(),
        child: false,
    }
}
fn declaration(name: &str) -> Value {
    json!({"name":name,"label":name,"description":"fixture", "parameters":{"type":"object"},
        "recordedCallPrefix":"recorded-","recordedError":"missing recording", "binding":{"kind":"builtin","name":name}})
}
async fn fixture(
    policy: ToolRegistrationPolicy,
) -> (HarnessActionHost, AgentHarness, tempfile::TempDir) {
    let cwd = tempfile::tempdir().unwrap();
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        cwd.path().into(),
        tokio::runtime::Handle::current(),
        BTreeMap::new(),
        Default::default(),
    );
    let harness = AgentHarness::create(AgentHarnessOptions {
        resources: AgentHarnessResources {
            tool_registration: policy,
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));
    (host, harness, cwd)
}
async fn request(
    host: &HarnessActionHost,
    action: &str,
    field: &str,
    value: Value,
) -> Result<Value, String> {
    host.native_request(
        owner(),
        json!({"port":"tools","action":action,field:value}),
        Default::default(),
    )
    .await
}
#[tokio::test]
async fn native_bindings_preserve_selection_provenance_and_atomic_ownership() {
    let (host, harness, _cwd) = fixture(Default::default()).await;
    let first = request(
        &host,
        "publish",
        "tools",
        json!([declaration("read"), declaration("write")]),
    )
    .await
    .unwrap();
    assert_eq!(first["tools"], json!(["read", "write"]));
    harness
        .select_native_tools(vec!["write".into()], Value::Null)
        .unwrap();
    let second = request(
        &host,
        "publish",
        "tools",
        json!([declaration("read"), declaration("grep")]),
    )
    .await
    .unwrap();
    assert_eq!(second["tools"], json!(["write", "grep"]));
    let inventory = request(&host, "inspect", "unused", Value::Null)
        .await
        .unwrap();
    assert!(inventory
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool["ownedBinding"] == true && tool["sourceInfo"]["source"] == "extension"));
    assert!(harness
        .inspect_tools()
        .unwrap()
        .iter()
        .all(|tool| tool.get("ownedBinding").is_none()));
    let mut other = owner();
    other.id += 1;
    let before = harness.native_context().unwrap();
    assert!(host.native_request(other, json!({"port":"tools","action":"publish","tools":[declaration("ls"),declaration("read")]}), Default::default()).await.unwrap_err().contains("another registration"));
    assert_eq!(before, harness.native_context().unwrap());
    assert!(!harness
        .inspect_tools()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "ls"));
    harness.close().await.unwrap();
    assert!(
        request(&host, "publish", "tools", json!([declaration("ls")]))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn native_binding_validation_and_tool_policy_never_expand_authority() {
    let (host, harness, _cwd) = fixture(ToolRegistrationPolicy {
        ceiling: None,
        disabled: false,
        allow: Some(vec!["read".into(), "write".into()]),
        deny: vec!["write".into()],
    })
    .await;
    assert_eq!(
        request(
            &host,
            "publish",
            "tools",
            json!([
                declaration("read"),
                declaration("write"),
                declaration("bash")
            ])
        )
        .await
        .unwrap()["registered"],
        json!(["read"])
    );
    harness.select_native_tools(vec![], Value::Null).unwrap();
    assert_eq!(
        request(&host, "publish", "tools", json!([])).await.unwrap()["tools"],
        json!(["read"])
    );
    let mut invalid = Vec::new();
    let mut item = declaration("read");
    item["binding"]["name"] = json!("bash");
    invalid.push(json!([item]));
    let mut item = declaration("read");
    item["sourceInfo"] = json!({"source":"builtin"});
    invalid.push(json!([item]));
    let mut item = declaration("read");
    item["parameters"] = Value::Null;
    invalid.push(json!([item]));
    let mut item = declaration("read");
    item["recordedError"] = Value::Null;
    invalid.push(json!([item]));
    invalid.push(json!([declaration("read"), declaration("read")]));
    invalid.push(json!(vec![declaration("read"); 129]));
    let before = harness.native_context().unwrap();
    for batch in invalid {
        assert!(request(&host, "publish", "tools", batch).await.is_err());
    }
    assert_eq!(before, harness.native_context().unwrap());
    let child = harness
        .create_child(
            crate::session::create_jsonl_session(_cwd.path(), _cwd.path().to_str().unwrap())
                .await
                .unwrap(),
            Arc::new(rpi_agent::CollectorEmitter::default()),
            1,
            Some(vec![]),
        )
        .await
        .unwrap();
    let mut child_owner = owner();
    child_owner.child = true;
    let result = child
        .execution_scope
        .run(host.native_request(
            child_owner,
            json!({"port":"tools","action":"publish","tools":[declaration("read")]}),
            Default::default(),
        ))
        .await
        .unwrap();
    assert_eq!(result["registered"], json!([]));
    assert!(child.harness.inspect_tools().unwrap().is_empty());
    assert_eq!(before, harness.native_context().unwrap());
    let (side, _) = harness
        .create_side_question(
            crate::session::create_jsonl_session(_cwd.path(), _cwd.path().to_str().unwrap())
                .await
                .unwrap(),
            Arc::new(rpi_agent::CollectorEmitter::default()),
        )
        .await
        .unwrap();
    let result = side
        .execution_scope
        .run(request(
            &host,
            "publish",
            "tools",
            json!([declaration("read")]),
        ))
        .await
        .unwrap();
    assert_eq!(result["registered"], json!([]));
    let (disabled, _disabled_harness, _cwd) = fixture(ToolRegistrationPolicy {
        disabled: true,
        ..Default::default()
    })
    .await;
    assert_eq!(
        request(&disabled, "publish", "tools", json!([declaration("read")]))
            .await
            .unwrap()["registered"],
        json!([])
    );
}
#[tokio::test]
async fn native_bound_tools_execute_once_and_recordings_execute_zero_times() {
    let (host, harness, cwd) = fixture(Default::default()).await;
    let mut edit = declaration("edit");
    edit["parameters"] = json!({"type":"object","properties":{"path":{"type":"string"},"oldText":{"type":"string"},"newText":{"type":"string"}},"required":["path","oldText","newText"],"additionalProperties":false});
    let recorded = json!({"name":"activity","label":"Activity","description":"Recorded", "parameters":{"type":"object"},"binding":{"kind":"recorded","error":"inert activity"}});
    request(&host, "publish", "tools", json!([edit, recorded]))
        .await
        .unwrap();
    std::fs::write(cwd.path().join("fixture.txt"), "before").unwrap();
    let call = |id: &str, recording: Option<Value>| {
        let mut value = json!({"id":id,"name":"edit","arguments":{"path":"fixture.txt","oldText":"before","newText":"beforeafter"}});
        if let Some(recording) = recording {
            value["recorded"] = recording;
        }
        value
    };
    for recording in [
        Some(
            json!({"version":1,"isError":false,"result":{"content":[{"type":"text","text":"recorded output"}],"details":{}}}),
        ),
        Some(json!({"version":1})),
        None,
    ] {
        let result = request(
            &host,
            "invoke",
            "call",
            call("recorded-1", recording.clone()),
        )
        .await
        .unwrap();
        assert_eq!(
            result["isError"],
            recording.as_ref().map_or(true, |v| v["isError"] != false)
        );
        assert_eq!(
            std::fs::read_to_string(cwd.path().join("fixture.txt")).unwrap(),
            "before"
        );
    }
    let result = request(&host, "invoke", "call", call("ordinary", None))
        .await
        .unwrap();
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("fixture.txt")).unwrap(),
        "beforeafter"
    );
    let ctx = ExecutionToolContext::new(host.execution_env(), Some(host.execution_env()));
    let mut modern = declaration("edit");
    modern["parameters"] = builtin(&ctx, Builtin::Edit)
        .schema()
        .parameters
        .as_value()
        .clone();
    modern["binding"]["prepareArguments"] = json!(true);
    request(&host, "publish", "tools", json!([modern]))
        .await
        .unwrap();
    std::fs::write(cwd.path().join("fixture.txt"), "before").unwrap();
    let result = request(&host, "invoke", "call", call("ordinary-modern", None))
        .await
        .unwrap();
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("fixture.txt")).unwrap(),
        "beforeafter"
    );
    let result = request(
        &host,
        "invoke",
        "call",
        json!({"id":"ordinary","name":"activity","arguments":{}}),
    )
    .await
    .unwrap();
    assert_eq!(result["isError"], true);
    assert!(result.to_string().contains("inert activity"));
    assert!(
        harness.get_tip_id().await.unwrap().is_none(),
        "host invocation must not start an agent turn"
    );
}

#[tokio::test]
async fn native_binding_refresh_uses_registry_order_and_capacity_failure_is_atomic() {
    let (host, harness, _cwd) = fixture(ToolRegistrationPolicy {
        allow: Some(vec!["read".into(), "write".into()]),
        ..Default::default()
    })
    .await;
    request(&host, "publish", "tools", json!([declaration("read")]))
        .await
        .unwrap();
    harness.select_native_tools(vec![], Value::Null).unwrap();
    assert_eq!(
        request(&host, "publish", "tools", json!([declaration("write")]))
            .await
            .unwrap()["tools"],
        json!(["read", "write"])
    );
    let (host, harness, _cwd) = fixture(Default::default()).await;
    let record = |n| json!({"name":format!("activity_{n}"),"label":"Activity","description":"Recorded","parameters":{},"binding":{"kind":"recorded","error":"inert"}});
    for start in [0, 128] {
        request(
            &host,
            "publish",
            "tools",
            json!((start..start + 128).map(record).collect::<Vec<_>>()),
        )
        .await
        .unwrap();
    }
    let before = harness.native_context().unwrap();
    assert!(request(&host, "publish", "tools", json!([record(256)]))
        .await
        .unwrap_err()
        .contains("capacity"));
    assert_eq!(harness.inspect_tools().unwrap().len(), 256);
    assert_eq!(harness.native_context().unwrap(), before);
}
