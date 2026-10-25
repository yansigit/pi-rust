use super::*;

struct RestoreConfig(Option<std::ffi::OsString>);
impl Drop for RestoreConfig {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
            None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
        }
    }
}

#[tokio::test]
async fn loaded_replacement_retires_old_authority_and_constructs_a_fresh_registration() {
    let temporary = tempfile::tempdir().unwrap();
    let _env = crate::config::test_support::SessionDirEnv::new(None);
    let _config = RestoreConfig(std::env::var_os(crate::config::CONFIG_DIR_ENV));
    let agent = temporary.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
    let source = temporary.path().join("plugin.rs");
    let audit = temporary.path().join("audit");
    let veto = temporary.path().join("veto");
    let fault = temporary.path().join("fault");
    let reject = temporary.path().join("reject");
    // Compile a real DLL with an awaited native callback, no SDK dependency or
    // subprocess cargo (the parent test is built with both supported toolchains).
    let fixture = r#"
use std::{ffi::c_void, io::Write, sync::atomic::{AtomicUsize, Ordering}};
#[repr(C)] #[derive(Clone, Copy)] struct Ref { ptr: *const u8, len: usize }
#[repr(C)] struct Owned { ptr: *mut u8, len: usize }
type Callback = extern "C" fn(Ref, *const c_void, *mut Owned, *mut c_void) -> i32;
extern "C" fn free(s: Owned) { if !s.ptr.is_null() { unsafe { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(s.ptr, s.len))); } } }
extern "C" fn callback(request: Ref, _: *const c_void, out: *mut Owned, generation: *mut c_void) -> i32 {
    let request = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(request.ptr, request.len)) };
    let tag: u32 = request.split("\"tag\":").nth(1).unwrap().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap();
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(AUDIT).unwrap();
    writeln!(log, "{}|{}|{}", generation as usize, tag, request).unwrap();
    if tag == 9 {
        if let Ok(path) = std::fs::read_to_string(FAULT) {
            std::fs::rename(&path, format!("{path}.bak")).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
    }
    let response = if [4, 5, 9].contains(&tag) && std::path::Path::new(VETO).exists() {
        r#RESPONSE#{"version":1,"result":{"cancel":true}}#RESPONSE#
    } else if tag == 9 {
        r#RESPONSE#{"version":1,"result":{"summary":{"summary":"loaded summary"},"label":"loaded label"}}#RESPONSE#
    } else { r#RESPONSE#{"version":1}#RESPONSE# };
    let mut bytes = response.as_bytes().to_vec().into_boxed_slice();
    unsafe { *out = Owned {ptr:bytes.as_mut_ptr(), len:bytes.len()}; }
    std::mem::forget(bytes); 0
}
static NEXT: AtomicUsize = AtomicUsize::new(0);
#[no_mangle] pub extern "C" fn rpi_plugin_register(api: *const u8) -> i32 {
    if std::path::Path::new(REJECT).exists() { return 1; }
    let generation = NEXT.fetch_add(1, Ordering::SeqCst) + 1;
    let register: extern "C" fn(u32, Callback, extern "C" fn(Owned), *mut c_void) -> i32 = unsafe {
        std::ptr::read(api.add(OFFSET).cast())
    };
    for tag in [2, 4, 5, 8, 9, 10, 38] { assert_eq!(register(tag, callback, free, generation as *mut c_void), 0); }
    0
}
"#.replace("r#RESPONSE#", "r#\"").replace("#RESPONSE#", "\"#");
    let fixture = format!("const AUDIT: &str = {:?}; const VETO: &str = {:?}; const FAULT: &str = {:?}; const REJECT: &str = {:?}; const OFFSET: usize = {};\n{fixture}",
        audit, veto, fault, reject, std::mem::offset_of!(rpi_plugin_sdk::PluginApi, register_event_result_handler));
    std::fs::write(&source, fixture).unwrap();
    let library = temporary.path().join(format!(
        "{}generation{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let output = std::process::Command::new("rustc")
        .args(["--crate-type=cdylib", "--edition=2021"])
        .arg(&source)
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let resolved = crate::provider::resolve(
        Some("anthropic"),
        Some(crate::provider::DEFAULT_MODEL_ID),
        None,
        Some("test-key"),
        None,
    )
    .unwrap();
    let args = Args {
        extension: vec![library],
        session_dir: Some(temporary.path().join("sessions")),
        no_tools: true,
        no_skills: true,
        no_prompt_templates: true,
        no_context_files: true,
        system_prompt: Some("test".into()),
        ..Default::default()
    };
    let cwd = temporary.path();
    let prepared = prepare_extensions(&args, cwd, false).await.unwrap();
    let (harness, _events, context) = build_prepared(&resolved, &args, cwd, false, prepared)
        .await
        .unwrap();
    let old_session = context.extension_session.lock().unwrap().clone();
    assert_eq!(old_session.loaded_paths().len(), 1);
    let old_snapshot = old_session.snapshot_arc().unwrap();
    let old_bridge = context.action_bridge.lock().unwrap().clone().unwrap();
    let host = old_bridge.clone_host();
    let old_id = context.tool_context.session_id();
    let factory = SessionRuntimeFactory::from_reload(&context);
    let admission = factory.try_admit().unwrap();
    assert!(host
        .new_session(serde_json::json!({}))
        .await
        .unwrap_err()
        .contains("already in progress"));
    drop(admission);
    assert!(old_bridge.is_active());
    assert_eq!(old_id, context.tool_context.session_id());
    let invalid = create_jsonl_session(
        &temporary.path().join("other-sessions"),
        temporary.path().join("missing-cwd").to_str().unwrap(),
    )
    .await
    .unwrap();
    let lane = harness.lane("main");
    let ctx = SessionReplacementContext {
        harness: &harness,
        lane: lane.as_ref(),
        extension_session: &context.extension_session,
        action_bridge: &context.action_bridge,
        tool_context: &context.tool_context,
        runtime_factory: Some(&factory),
    };
    assert!(
        replace_active_session(&ctx, invalid, SessionReplacementReason::Resume, None)
            .await
            .unwrap_err()
            .contains("working directory does not exist")
    );
    assert!(
        old_bridge.is_active(),
        "destination preparation must precede retirement"
    );
    assert!(!old_session.shutdown_signal().is_cancelled());
    // A veto occurs before destination open/create, and does not retire anything.
    std::fs::write(&veto, "cancel").unwrap();
    let cancelled = replace_host_session(
        &harness,
        &context,
        HostSessionRequest::Resume("not-a-session".into()),
    )
    .await
    .unwrap();
    assert!(matches!(cancelled, SessionReplacement::Cancelled { .. }));
    assert!(host
        .new_session(serde_json::json!({}))
        .await
        .unwrap_err()
        .contains("cancelled"));
    assert!(old_bridge.is_active());
    std::fs::remove_file(&veto).unwrap();
    host.new_session(serde_json::json!({})).await.unwrap();
    assert_ne!(old_id, context.tool_context.session_id());
    assert!(
        !old_bridge.is_active(),
        "old command/native contexts must be stale"
    );
    assert!(!old_snapshot.active_flag().load(Ordering::SeqCst));
    assert!(old_session.shutdown_signal().is_cancelled());
    let fresh = context.extension_session.lock().unwrap().clone();
    assert_eq!(fresh.loaded_paths().len(), 1);
    assert!(!fresh.shutdown_signal().is_cancelled());
    assert!(fresh
        .snapshot_arc()
        .unwrap()
        .active_flag()
        .load(Ordering::SeqCst));
    let fresh_bridge = context.action_bridge.lock().unwrap().clone().unwrap();
    assert!(fresh_bridge.is_active());
    assert!(!Arc::ptr_eq(&old_bridge, &fresh_bridge));
    // A second replacement is also constructed from the factory, not the closed runtime.
    fresh_bridge
        .clone_host()
        .new_session(serde_json::json!({}))
        .await
        .unwrap();
    assert!(!fresh_bridge.is_active());
    assert!(context
        .action_bridge
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_active());
    let source_id = harness.session().storage().metadata().id.clone();
    let anchor = harness
        .session()
        .append_message(rpi_agent::AgentMessage::User(rpi_ai::UserMessage::new(
            "fork-anchor",
            0,
        )))
        .await
        .unwrap();
    harness.session().sync().await.unwrap();
    replace_host_session(
        &harness,
        &context,
        HostSessionRequest::Fork {
            entry_id: Some(anchor.clone()),
            position: rpi_harness::session::types::ForkPosition::Before,
        },
    )
    .await
    .unwrap();
    assert!(harness
        .session()
        .get_entry(&anchor)
        .await
        .unwrap()
        .is_none());
    replace_host_session(&harness, &context, HostSessionRequest::Resume(source_id))
        .await
        .unwrap();
    replace_host_session(
        &harness,
        &context,
        HostSessionRequest::Fork {
            entry_id: Some(anchor.clone()),
            position: rpi_harness::session::types::ForkPosition::At,
        },
    )
    .await
    .unwrap();
    assert!(harness
        .session()
        .get_entry(&anchor)
        .await
        .unwrap()
        .is_some());
    // Loaded tree handlers run on the fresh authority, not the closed runtime.
    let tree_bridge = context.action_bridge.lock().unwrap().clone().unwrap();
    let tree_host = tree_bridge.clone_host();
    let left = harness
        .session()
        .append_message(rpi_agent::AgentMessage::User(rpi_ai::UserMessage::new(
            "left", 0,
        )))
        .await
        .unwrap();
    harness
        .session()
        .move_lane("main", Some(&anchor))
        .await
        .unwrap();
    let right = harness
        .session()
        .append_message(rpi_agent::AgentMessage::User(rpi_ai::UserMessage::new(
            "right", 0,
        )))
        .await
        .unwrap();
    harness
        .session()
        .move_lane("main", Some(&left))
        .await
        .unwrap();
    std::fs::write(&veto, "cancel tree").unwrap();
    let cancelled = tree_host
        .navigate_tree(serde_json::json!({"targetId":right}))
        .await
        .unwrap();
    assert_eq!(cancelled["cancelled"], true);
    assert_eq!(
        harness.lane("main").get_leaf_id().await.unwrap().as_deref(),
        Some(left.as_str())
    );
    std::fs::remove_file(&veto).unwrap();
    let completed = tree_host
        .navigate_tree(serde_json::json!({"targetId":right,"summarize":true}))
        .await
        .unwrap();
    assert_eq!(completed["cancelled"], false);
    assert!(
        tree_bridge.is_active(),
        "navigation must not replace runtime authority"
    );
    let entries = harness
        .session()
        .find_entries(&rpi_harness::session::types::EntryQuery::default())
        .await
        .unwrap();
    let serialized = serde_json::to_string(&entries).unwrap();
    assert!(serialized.contains("loaded summary"));
    let summary_leaf = harness.lane("main").get_leaf_id().await.unwrap().unwrap();
    assert_eq!(
        harness
            .session()
            .get_label(&summary_leaf)
            .await
            .unwrap()
            .as_deref(),
        Some("loaded label")
    );
    // Fault the real JSONL path after the before-tree hook. No success event may
    // be emitted if summary/label persistence fails. Restore the disk afterwards.
    harness
        .session()
        .move_lane("main", Some(&left))
        .await
        .unwrap();
    let path = harness
        .session()
        .storage()
        .location()
        .await
        .unwrap()
        .unwrap();
    std::fs::write(&fault, &path).unwrap();
    let audit_start = std::fs::read_to_string(&audit).unwrap().len();
    assert!(tree_host
        .navigate_tree(serde_json::json!({"targetId":right,"summarize":true}))
        .await
        .is_err());
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(format!("{path}.bak"), &path).unwrap();
    std::fs::remove_file(&fault).unwrap();
    let log = std::fs::read_to_string(&audit).unwrap();
    let tail = &log[audit_start..];
    assert!(
        !tail
            .lines()
            .any(|line| line.split('|').nth(1) == Some("10")),
        "{tail}"
    );
    assert!(
        tail.lines()
            .any(|line| line.split('|').nth(1) == Some("38")),
        "{tail}"
    );
    // Registration memory is library-owned; session generation still changes.
    let rows: Vec<(usize, u32)> = log
        .lines()
        .map(|line| {
            let mut cells = line.split('|');
            (
                cells.next().unwrap().parse().unwrap(),
                cells.next().unwrap().parse().unwrap(),
            )
        })
        .collect();
    let first_shutdown = rows.iter().position(|(_, tag)| *tag == 8).unwrap();
    assert!(rows[first_shutdown + 1].0 > rows[first_shutdown].0);
    assert_eq!(rows[first_shutdown + 1].1, 2);
    let native_events = rows
        .iter()
        .filter(|(_, tag)| [9, 10, 38].contains(tag))
        .collect::<Vec<_>>();
    assert!(native_events
        .iter()
        .all(|(generation, _)| *generation == rows.last().unwrap().0));
    // Registration failure is fail-closed, never a revived closed registration.
    // The staged durable session stays selected and the host can retry safely.
    std::fs::write(&reject, "reject fresh registration").unwrap();
    let previous = context.tool_context.session_id();
    assert!(
        replace_host_session(&harness, &context, HostSessionRequest::New)
            .await
            .unwrap_err()
            .contains("Failed to construct fresh extension session runtime")
    );
    assert_ne!(previous, context.tool_context.session_id());
    assert!(!tree_bridge.is_active());
    std::fs::remove_file(&reject).unwrap();
    replace_host_session(&harness, &context, HostSessionRequest::New)
        .await
        .unwrap();
    assert!(context
        .action_bridge
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_active());
}
