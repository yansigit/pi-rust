//! B5c — `PluggableProvider`: the host-side `Provider` impl wrapping a plugin's
//! `ProviderRequestFn`.
//!
//! A plugin's [`ProviderRequestFn`](rpi_plugin_sdk::ProviderRequestFn) is a
//! **synchronous one-shot**: given a borrowed JSON request envelope it writes a
//! **plugin-owned** `out` [`StbString`] (a full assistant-message JSON). It
//! cannot drive a chunked `stream_simple` without blocking, so the v1
//! `PluggableProvider` emits the full response as a **single terminal `Done`
//! chunk** — a documented divergence from pi's async `streamSimple`
//! (`custom-provider.md`). The ffi call runs on `spawn_blocking` (via a captured
//! [`Handle`](tokio::runtime::Handle) — same foreign-thread fix as the
//! `runtime_action` bridge), so the async `stream_simple` never blocks a runtime
//! worker.
//!
//! ## Request/response envelope
//!
//! - **Request** (borrowed `StbStringRef` handed to `request_fn`): JSON
//!   `{ model, context, options }`, where `model`/`context` are the serde
//!   serializations of [`Model`](rpi_ai::Model)/[`Context`](rpi_ai::Context)
//!   and `options` is a hand-built JSON of the [`SimpleStreamOptions`] fields
//!   (that type is not `Serialize`; we mirror the field set
//!   [`ExtensionProviderHooks`](crate::ExtensionProviderHooks) dispatches).
//! - **Response** (the plugin-owned `out` `StbString`): an
//!   [`AssistantMessage`] JSON. The host reclaims `out` via the plugin's
//!   `plugin_free_string` (traveled with the registration), parses the JSON, and
//!   pushes it as one `Done` (or `Error` on nonzero rc / parse failure).
//!
//! ## Lifetime + staleness
//!
//! `PluggableProvider` mirrors [`ExtensionProviderHooks`]: it holds an
//! `Arc<RegistrySnapshot>` (the staleness flag is shared — a `/reload`
//! invalidate marks it stale) + an `Arc<PluginKeepalive>` (keeps the cdylib
//! mapped so `request_fn` stays callable) + the [`RegisteredProvider`] record +
//! a `Handle` captured at build time (the host is on the runtime when it
//! constructs providers from the session).

use rpi_ai::provider_wire::{encode_options as options_json, StreamMapper};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rpi_ai::event_stream::create_assistant_message_event_stream_with_cancellation;
use rpi_ai::types::{AssistantMessage, DoneReason, ErrorReason};
#[cfg(test)]
use rpi_ai::{event_stream::create_assistant_message_event_stream, Content};
use rpi_ai::{
    AssistantMessageEvent, AssistantMessageEventStream, AssistantMessageEventStreamProducer,
    Context, Model, Provider, SimpleStreamOptions,
};
use rpi_plugin_sdk::callbacks::OwnedString;
use rpi_plugin_sdk::{StbString, StbStringRef};
use tokio::runtime::Handle;

use crate::loader::ExtensionSession;
use crate::registry::{RegisteredProvider, RegistrySnapshot};
use crate::PluginKeepalive;

#[cfg(all(test, feature = "provider-performance"))]
mod compact_tests;
pub mod delegation;
#[cfg(test)]
mod lifetime_tests;
#[cfg(all(test, feature = "provider-performance"))]
mod performance;
mod queue;
#[cfg(test)]
mod stream_tests;
pub mod wrappers;

/// Reserve child capacity before spawning the producer; move this ownership
/// into the foreign driver so runtime shutdown cannot unload executing code.
struct ProviderRequestLifetime {
    invocation: Option<Arc<crate::ActionBridge>>,
    _keepalive: Arc<PluginKeepalive>,
    _background: Option<rpi_agent::tool_execution_scope::BackgroundToolGuard>,
    continuation: Option<Arc<wrappers::Continuation>>,
}
impl ProviderRequestLifetime {
    fn new(keepalive: Arc<PluginKeepalive>) -> Self {
        Self {
            _keepalive: keepalive,
            _background: rpi_agent::tool_execution_scope::track_background_tool(),
            continuation: None,
            invocation: None,
        }
    }
}

/// A `Provider` backed by a plugin's sync `ProviderRequestFn`.
///
/// See the module docs for the v1 one-shot streaming model + the request/response
/// envelope. Built from a loaded [`ExtensionSession`]; one `PluggableProvider`
/// per [`RegisteredProvider`] in the session's snapshot.
pub struct PluggableProvider {
    /// The registration record (id/base_url/api_style + the fn pointers). Cloned
    /// from the snapshot; the fn pointers are valid for the keepalive's lifetime.
    record: RegisteredProvider,
    /// Shared staleness flag (a `/reload` invalidate flows through here).
    snapshot: Arc<RegistrySnapshot>,
    /// Keeps the cdylib mapped so `request_fn` stays callable.
    _keepalive: Arc<PluginKeepalive>,
    /// Captured at build time — `spawn_blocking` works from the async context.
    runtime: Handle,
    models: Vec<Model>,
    wrapper: Option<wrappers::WrapperNext>,
    operations: Option<Arc<crate::provider_runtime::NativeProviderOperations>>,
}

impl PluggableProvider {
    fn new(
        record: RegisteredProvider,
        snapshot: Arc<RegistrySnapshot>,
        keepalive: Arc<PluginKeepalive>,
        runtime: Handle,
    ) -> Arc<Self> {
        let models = snapshot
            .provider_manifest(&record.provider_id)
            .map(|manifest| manifest.models.clone())
            .unwrap_or_default();
        let operations = crate::provider_runtime::NativeProviderOperations::from_parts(
            snapshot.clone(),
            keepalive.clone(),
            &record.provider_id,
        )
        .map(Arc::new);
        Arc::new(Self {
            operations,
            record,
            snapshot,
            _keepalive: keepalive,
            runtime,
            models,
            wrapper: None,
        })
    }

    /// Called only after the host verifies ownership of the source implementation.
    pub fn from_implementation(
        session: &ExtensionSession,
        source: &str,
        id: &str,
        models: Vec<Model>,
    ) -> Option<Arc<dyn Provider>> {
        let snapshot = session.snapshot_arc()?;
        let mut record = snapshot
            .providers()
            .iter()
            .find(|record| record.provider_id == source)?
            .clone();
        if models
            .iter()
            .any(|model| model.provider != id || model.api.as_str() != record.api_style)
        {
            return None;
        }
        record.provider_id = id.into();
        Some(Arc::new(Self {
            record,
            snapshot,
            _keepalive: session.keepalive(),
            runtime: Handle::current(),
            models,
            wrapper: None,
            operations: crate::provider_runtime::NativeProviderOperations::from_session(
                session, source,
            )
            .map(Arc::new),
        }))
    }

    /// Build one [`PluggableProvider`] per registered provider in the session's
    /// snapshot, as `Arc<dyn Provider>`. Returns an empty vec when no plugin
    /// registered a provider (so a session without provider plugins injects
    /// nothing). The `Handle` MUST be captured from a thread running the target
    /// runtime (pi-cli builds providers on the async main thread, same as the
    /// `ActionBridge`).
    pub fn from_session(session: &ExtensionSession, runtime: Handle) -> Vec<Arc<dyn Provider>> {
        let Some(snapshot) = session.snapshot_arc() else {
            return Vec::new();
        };
        let keepalive = session.keepalive();
        snapshot
            .providers()
            .iter()
            .cloned()
            .map(|record| {
                Self::new(
                    record,
                    Arc::clone(&snapshot),
                    Arc::clone(&keepalive),
                    runtime.clone(),
                ) as Arc<dyn Provider>
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl Provider for PluggableProvider {
    fn id(&self) -> &str {
        &self.record.provider_id
    }

    fn models(&self) -> &[Model] {
        &self.models
    }

    fn with_catalog(&self, models: Vec<Model>) -> Option<Arc<dyn Provider>> {
        if self.wrapper.is_some()
            || models.iter().any(|model| {
                model.provider != self.record.provider_id
                    || model.api.as_str() != self.record.api_style
            })
        {
            return None;
        }
        Some(Arc::new(Self {
            record: self.record.clone(),
            snapshot: self.snapshot.clone(),
            _keepalive: self._keepalive.clone(),
            runtime: self.runtime.clone(),
            models,
            wrapper: None,
            operations: self.operations.clone(),
        }))
    }
    async fn preflight(
        &self,
        model: &Model,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        if let Some(wrapper) = &self.wrapper {
            return wrapper.preflight(model, signal).await;
        }
        if let Some(operations) = &self.operations {
            if operations.supports("preflight") {
                operations
                    .request(
                        "preflight",
                        serde_json::json!({"modelId":model.id,"api":model.api}),
                        signal,
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut prod, stream) =
            create_assistant_message_event_stream_with_cancellation(&opts.signal);
        let snapshot = Arc::clone(&self.snapshot);
        // into_owned_record copies the fn pointers + owned strings; the
        // `user_data` raw pointer is carried (plugin-owned, valid for the
        // keepalive lifetime).
        let record = self.record.clone();
        let runtime = self.runtime.clone();
        let mut lifetime = ProviderRequestLifetime::new(Arc::clone(&self._keepalive));
        let model = model.clone();
        let ctx = Arc::new(ctx.clone());
        // `opts` is not `Clone`-free of lifetime — capture the fields the
        // request envelope needs as owned values so the spawned task is
        // `'static`. `SimpleStreamOptions` IS `Clone` (verified, provider.rs:27),
        // so a single clone carries every field into the task.
        let mut opts = opts.clone();
        opts.signal = prod.cancellation_token();
        let shutdown = self._keepalive.shutdown_signal();
        let admission = opts.signal.clone();
        self.runtime.spawn(async move {
            tokio::select! { biased;
                _ = admission.cancelled() => {},
                _ = shutdown.cancelled() => admission.cancel(),
            }
        });
        lifetime.invocation = self.record.action_bridge.as_ref().map(|bridge| {
            let mut invocation = bridge.for_invocation(
                rpi_agent::tool_execution_scope::is_child_execution(),
                opts.signal.clone(),
            );
            if let Some(identity) = &opts.identity {
                Arc::get_mut(&mut invocation).unwrap().operation = Some(identity.clone());
            }
            invocation
        });
        if let Some(wrapper) = &self.wrapper {
            match wrapper.continuation(&model, &ctx, &opts) {
                Ok(continuation) => lifetime.continuation = Some(continuation),
                Err(error) => {
                    push_terminal(&mut prod, error_message(&record, &error)).await;
                    return stream;
                }
            }
        }

        // `spawn` (not `spawn_blocking`) for the producer task — the producer
        // must be async so it can `await` the blocking ffi call. Inside, we
        // `spawn_blocking` the sync `request_fn` (the ffi call is blocking by
        // contract). This keeps the runtime worker free while the plugin runs.
        tokio::spawn(async move {
            if let Some(stream_fn) = record.stream_fn {
                // Streaming provider: the plugin pushes per-chunk envelopes on
                // the callback while its request fn is still running; the
                // producer forwards each one immediately, so the consumer sees
                // deltas as they arrive instead of one terminal chunk.
                let message = drive_streaming_plugin_provider(
                    &snapshot, &record, stream_fn, runtime, lifetime, &model, &ctx, &opts,
                    &mut prod,
                )
                .await;
                push_terminal(&mut prod, message).await;
            } else {
                let message = drive_plugin_provider(
                    &snapshot, &record, runtime, lifetime, &model, &ctx, &opts,
                )
                .await;
                push_terminal(&mut prod, message).await;
            }
            // `prod` drops here, closing the mpsc sender so the consumer's
            // `next()` returns `None` after the terminal event.
        });

        stream
    }
}

/// Run the sync `ProviderRequestFn` on `spawn_blocking`, read its plugin-owned
/// `out` JSON, reclaim it via `plugin_free_string`, parse to an
/// [`AssistantMessage`]. Returns an error terminal message on any failure
/// (staleness, nonzero rc, parse error, runtime shutdown).
async fn drive_plugin_provider(
    snapshot: &Arc<RegistrySnapshot>,
    record: &RegisteredProvider,
    runtime: Handle,
    lifetime: ProviderRequestLifetime,
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
) -> AssistantMessage {
    // Staleness: a `/reload`d session must not drive its old providers.
    if !snapshot.is_active() {
        return AssistantMessage::terminal(
            model.api.clone(),
            record.provider_id.clone(),
            model.id.clone(),
            rpi_ai::types::StopReason::Error,
            "extensions provider registry is stale (session swapped/reloaded)",
            0,
        );
    }

    // Build the request envelope JSON. `Model`/`Context` are Serialize;
    // `SimpleStreamOptions` is not, so the `options` object is hand-built
    // (mirrors `ExtensionProviderHooks::before_request`).
    let request = serde_json::json!({
        "model": serde_json::to_value(model).unwrap_or(serde_json::Value::Null),
        "context": serde_json::to_value(ctx).unwrap_or(serde_json::Value::Null),
        "options": options_json(opts),
    });
    let request_json = request.to_string();

    // The fn pointers + user_data are `Copy`-able (fn ptrs) or plain raw (ud);
    // clone what spawn_blocking needs. `record` is `Clone` (owned strings).
    let record = record.clone();
    let provider_id = record.provider_id.clone();
    let api = model.api.clone();
    let model_id = model.id.clone();

    // `spawn_blocking` the sync ffi call — `request_fn` is blocking by contract.
    // The join handle is awaited so a panic in the plugin (caught by
    // `catch_unwind` below) surfaces as an error, not a silent hang.
    let request_model_id = model_id.clone();
    let join = runtime.spawn_blocking(move || {
        let _lifetime = lifetime;
        let _invocation = _lifetime
            .invocation
            .as_ref()
            .map(|bridge| bridge.enter_invocation());
        run_provider_request(&record, &request_json, &request_model_id)
    });
    let outcome = match join.await {
        Ok(inner) => inner,
        Err(join_err) => {
            // The blocking task panicked before returning (catch_unwind inside
            // caught a plugin panic and aborted, OR tokio cancelled). Either way
            // no response.
            return AssistantMessage::terminal(
                api,
                provider_id,
                model_id,
                rpi_ai::types::StopReason::Error,
                format!("plugin provider task failed: {join_err}"),
                0,
            );
        }
    };

    match outcome {
        ProviderOutcome::Ok(message) => message,
        ProviderOutcome::Err(msg) => AssistantMessage::terminal(
            api,
            provider_id,
            model_id,
            rpi_ai::types::StopReason::Error,
            msg,
            0,
        ),
    }
}

/// The result of one sync `request_fn` call: either a parsed assistant message
/// or an error string. Computed inside `spawn_blocking`.
enum ProviderOutcome {
    Ok(AssistantMessage),
    Err(String),
}

/// The inner sync driver (runs on the blocking pool). Calls `request_fn`,
/// recovers the plugin-owned `out` `StbString`, reclaims it via
/// `plugin_free_string`, parses to an [`AssistantMessage`]. Wrapped in
/// `catch_unwind` so a plugin panic cannot unwind across FFI (⇒ error outcome).
fn run_provider_request(
    record: &RegisteredProvider,
    request_json: &str,
    model_id: &str,
) -> ProviderOutcome {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_provider_request_inner(record, request_json, model_id)
    }));
    match outcome {
        Ok(inner) => inner,
        Err(_) => {
            tracing::error!(
                "plugin provider {} panicked — refusing to unwind across FFI",
                record.provider_id
            );
            ProviderOutcome::Err(format!(
                "plugin provider {} panicked during request",
                record.provider_id
            ))
        }
    }
}

fn run_provider_request_inner(
    record: &RegisteredProvider,
    request_json: &str,
    model_id: &str,
) -> ProviderOutcome {
    // Borrowed request envelope: the plugin must NOT free it (StbStringRef is a
    // borrow — the SDK contract).
    let req_ref = StbStringRef::from_str(request_json);

    // Plugin-owned `out` — `request_fn` writes a StbString into `*out` (plugin
    // allocation, reclaimed via `plugin_free_string`). Start empty.
    let mut out = StbString::empty();
    let rc = (record.request_fn)(req_ref, &mut out as *mut StbString, record.user_data);
    // SAFETY: the callback transfers its live output to its registered producer.
    let out = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(record.plugin_free_string))
    };
    if rc != 0 {
        // Nonzero rc: the plugin signaled failure. Reclaim whatever it wrote
        // (may be empty) before returning.
        return ProviderOutcome::Err(format!(
            "plugin provider {} returned error code {rc}",
            record.provider_id
        ));
    }

    // Read the plugin-owned JSON (borrow `out` for the parse) then reclaim it.
    let response_text = out.to_string_lossy();

    // Parse the response as an AssistantMessage. A plugin that returns a partial
    // or malformed message surfaces a parse error (not a crash) — the host
    // emits it as an `Error` terminal so the run terminates cleanly.
    match serde_json::from_str::<AssistantMessage>(&response_text) {
        Ok(message) => {
            // Stamp the provider/model identity the host expects if the plugin
            // omitted them (a plugin may return only `content`/`stopReason`).
            // We do NOT overwrite fields the plugin set — only fill defaults on
            // a message whose provider/model don't match this provider.
            let mut message = message;
            if message.provider.is_empty() {
                message.provider = record.provider_id.clone();
            }
            if message.model.is_empty() {
                message.model = model_id.to_string();
            }
            ProviderOutcome::Ok(message)
        }
        Err(err) => ProviderOutcome::Err(format!(
            "plugin provider {} returned unparseable response: {err}",
            record.provider_id
        )),
    }
}

/// Push the terminal event for `message`. A `StopReason::Error`/`Aborted`
/// message → `Error` (with the matching `ErrorReason`); otherwise → `Done`
/// (mapping the stop reason onto [`DoneReason`]). This is the single chunk the
/// one-shot path emits, and the terminal event for the streaming path.
async fn push_terminal(prod: &mut AssistantMessageEventStreamProducer, message: AssistantMessage) {
    use rpi_ai::types::StopReason;
    match message.stop_reason {
        // The two failure reasons map to distinct `ErrorReason`s: `Aborted` is
        // the caller going away (a cancellation), not a provider fault. The agent
        // loop and the TUI distinguish them, so collapsing both onto `Error`
        // would report a user-initiated cancel as a provider failure.
        StopReason::Error | StopReason::Aborted => {
            let reason = if message.stop_reason == StopReason::Aborted {
                ErrorReason::Aborted
            } else {
                ErrorReason::Error
            };
            prod.push(AssistantMessageEvent::Error {
                reason,
                error: message,
            })
            .await;
        }
        other => {
            let reason = match other {
                StopReason::Stop => DoneReason::Stop,
                StopReason::Length => DoneReason::Length,
                StopReason::ToolUse => DoneReason::ToolUse,
                StopReason::Deferred => DoneReason::Deferred,
                // Pending/unknown settle to Stop (the plugin should have set a
                // terminal reason; if not, the response is still complete).
                _ => DoneReason::Stop,
            };
            prod.push(AssistantMessageEvent::Done { reason, message })
                .await;
        }
    }
}

/// Build the `options` JSON for the request envelope. `SimpleStreamOptions` is
/// not `Serialize`, so we hand-build the field set (mirrors
/// `ExtensionProviderHooks::before_request`). Sensitive fields (`api_key`) are
/// included — the plugin is trusted host-side code (a cdylib the user installed
/// under `--extensions-dir`); this matches pi's `registerProvider` handing the
/// full options to the plugin's `streamSimple`.
// ---------------------------------------------------------------------------
// Streaming providers (B5c-s) — the per-chunk callback bridge
// ---------------------------------------------------------------------------

/// Run a plugin's [`ProviderStreamRequestFn`](rpi_plugin_sdk::ProviderStreamRequestFn)
/// on `spawn_blocking` while forwarding every emitted chunk to `prod` **as it
/// arrives**, then return the plugin's terminal message.
///
/// The forwarding is the whole point: the request fn is synchronous, so the
/// callback is the only moment the host learns about a delta. It runs on the
/// blocking-pool thread, inside the plugin's call, and queues chunks for the
/// async driver to forward — which is what makes the consumer's `next()`
/// resolve while the model is still producing.
///
/// Ordering guarantee: chunks are pushed from the single blocking thread, in the
/// order the plugin emits them. On completion the caller waits for the plugin's
/// final message; on cancellation it closes the chunk channel immediately. No
/// chunk can therefore land after the terminal event.
///
/// Cancellation: the caller's signal is selected against incoming chunks and
/// the final join. When it fires the driver (a) latches the control state so the
/// plugin can see it, (b) drops the chunk channel so the plugin's next `emit`
/// reports a rejected chunk, and (c) returns immediately with an `Aborted`
/// terminal rather than waiting out a generation nobody will read. The blocking
/// call itself cannot be interrupted mid-FFI; a plugin that honors the latch
/// unwinds promptly, and one that does not still terminates the run.
#[allow(clippy::too_many_arguments)]
async fn drive_streaming_plugin_provider(
    snapshot: &Arc<RegistrySnapshot>,
    record: &RegisteredProvider,
    stream_fn: rpi_plugin_sdk::ProviderStreamRequestFn,
    runtime: Handle,
    lifetime: ProviderRequestLifetime,
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
    prod: &mut AssistantMessageEventStreamProducer,
) -> AssistantMessage {
    // Staleness: a `/reload`d session must not drive its old providers.
    if !snapshot.is_active() {
        return AssistantMessage::terminal(
            model.api.clone(),
            record.provider_id.clone(),
            model.id.clone(),
            rpi_ai::types::StopReason::Error,
            "extensions provider registry is stale (session swapped/reloaded)",
            0,
        );
    }

    // `options.signal` is the same cancellation contract the built-in providers
    // honor: the agent loop cancels this token when the user aborts the run.
    let cancel = opts.signal.clone();
    let request_json = if let Some(continuation) = &lifetime.continuation {
        let mut request =
            serde_json::json!({"model":model,"context":ctx,"options":options_json(opts)});
        request["__rpiContinuation"] = continuation.descriptor();
        request.to_string()
    } else {
        request_envelope_json(model, ctx, opts)
    };

    // The callback bridge. It is moved (not shared) into the blocking task, and
    // it owns the chunk channel's **only** sender: when that task drops it, the
    // consumer's `recv()` sees the channel close. That lifetime coupling is the
    // reason this is a plain field and not a second `Arc` handle.
    let control = Arc::new(ProviderControlState::default());
    let (tx, mut rx) = queue::channel(64, 4 * 1024 * 1024, Arc::clone(&control));
    let bridge = StreamEmitBridge {
        tx,
        plugin_free_string: record.plugin_free_string,
        control: Some(Arc::clone(&control)),
    };
    let owner = Arc::new(ProviderControlOwner {
        state: Arc::clone(&control),
    });

    let record_owned = record.clone();
    let snapshot_owned = Arc::clone(snapshot);
    let owner_owned = Arc::clone(&owner);
    let api = model.api.clone();
    let provider_id = record.provider_id.clone();
    let model_id = model.id.clone();

    // The callback blocks on bridge capacity while this task forwards events.
    let request_model_id = model_id.clone();
    let join = runtime.spawn_blocking(move || {
        // Cancellation detaches this task; keep the library and host-side
        // context alive until the FFI call and all plugin-owned frees finish.
        let _lifetime = lifetime;
        let _invocation = _lifetime
            .invocation
            .as_ref()
            .map(|bridge| bridge.enter_invocation());
        run_streaming_provider_request(
            &snapshot_owned,
            &record_owned,
            stream_fn,
            &bridge,
            &owner_owned,
            &request_json,
            &request_model_id,
        )
    });

    // One async task maps and forwards chunks; pressure propagates to emit.
    let mut mapper = StreamMapper::new(
        model.api.clone(),
        record.provider_id.clone(),
        model.id.clone(),
    );

    let mut cancelled_mid_stream = false;
    // Forward every chunk, in arrival order, until the channel closes (the
    // blocking task dropping its sender). A chunk the mapper parks behind the
    // host-emitted `Start` is drained on the spot, so ordering holds without a
    // second queue. The cancel arm can fire at any await point — including on the
    // very first `recv`, which is why it is selected rather than checked in the
    // loop body.
    'forward: loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Latch first: the plugin may be mid-generation and its next
                // poll of `is_cancelled` must see this, not a later race.
                control.cancel();
                cancelled_mid_stream = true;
                break;
            }
            chunk = rx.recv() => match chunk {
                Some(chunk) => {
                    if chunk.get("type").and_then(serde_json::Value::as_str) == Some("response") {
                        if let Some(observer) = &opts.response_observer {
                            let receipt = serde_json::from_value::<rpi_ai::provider::ResponseMetadata>(chunk["response"].clone());
                            let Ok(receipt) = receipt else {
                                control.cancel();
                                let mut partial = mapper.output.clone();
                                partial.stop_reason = rpi_ai::types::StopReason::Error;
                                partial.error_message = Some("invalid private provider response metadata".into());
                                return partial;
                            };
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    control.cancel();
                                    cancelled_mid_stream = true;
                                    break 'forward;
                                },
                                _ = observer(receipt) => {},
                            }
                        }
                        continue;
                    }
                    let mut next = Some(chunk);
                    while let Some(chunk) = next {
                        let metadata_only = chunk.get("type").and_then(serde_json::Value::as_str) == Some("usage");
                        if let Some(event) = mapper.apply(chunk) {
                            let accepted = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => false,
                                accepted = prod.push(event) => accepted,
                            };
                            if !accepted {
                                control.cancel();
                                cancelled_mid_stream = true;
                                mapper.output = prod.committed_message().cloned().unwrap_or_else(||
                                    AssistantMessage::empty(model.api.clone(), record.provider_id.clone(), model.id.clone(), 0));
                                break 'forward;
                            }
                        } else if metadata_only {
                            prod.commit_metadata(&mapper.output);
                        }
                        next = mapper.take_deferred();
                    }
                }
                // The plugin finished (or its task ended). Stop forwarding: the
                // terminal event is pushed below, after the join resolves.
                None => break,
            },
        }
    }

    // Normal completion has already drained the channel. Cancellation discards
    // queued chunks so no more content is forwarded after the cancel signal.
    rx.close();
    let mut message = if cancelled_mid_stream {
        // Dropping the join handle detaches the blocking task; waiting here
        // would let an uncooperative plugin hold the entire agent run open.
        mapper.aborted_message()
    } else {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                control.cancel();
                mapper.aborted_message()
            }
            outcome = join => match outcome {
                Ok(message) => message,
                Err(join_err) => AssistantMessage::terminal(
                    api,
                    provider_id,
                    model_id,
                    rpi_ai::types::StopReason::Error,
                    format!("plugin provider task failed: {join_err}"),
                    0,
                ),
            },
        }
    };
    if message.provider.is_empty() {
        message.provider = record.provider_id.clone();
    }
    if message.model.is_empty() {
        message.model = model.id.clone();
    }
    for event in mapper.chunks_then_message(&message) {
        prod.push(event).await;
    }
    message
}

/// The terminal result of driving a streaming provider: the plugin's own final
/// message, which becomes the `Done`/`Error` payload.
type StreamingProviderOutcome = AssistantMessage;

/// Cancellation latched from `options.signal` for the plugin's synchronous
/// polling and blocking waits. The condition variable wakes every waiter when
/// the async driver observes cancellation.
#[derive(Default)]
struct ProviderControlState {
    cancelled: AtomicBool,
    wait_lock: Mutex<()>,
    wake: Condvar,
    queue: Mutex<Option<std::sync::Weak<queue::Queue>>>,
}

impl ProviderControlState {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Latch cancellation. Idempotent and thread-safe; the plugin may observe it
    /// at any time after this returns.
    fn cancel(&self) {
        // Serialize with the waiter's predicate check to avoid a lost wakeup.
        let _guard = self.wait_lock.lock().unwrap_or_else(|err| err.into_inner());
        self.cancelled.store(true, Ordering::SeqCst);
        self.wake.notify_all();
        if let Some(queue) = self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        {
            queue.cancel();
        }
    }

    /// Block up to `timeout_ms` for cancellation. `0` is a non-blocking poll.
    /// Runs on the plugin's request thread and only touches this struct, so it
    /// cannot deadlock on any host lock.
    fn wait(&self, timeout_ms: u32) -> bool {
        if self.is_cancelled() {
            return true;
        }
        if timeout_ms == 0 {
            return false;
        }
        let guard = self.wait_lock.lock().unwrap_or_else(|err| err.into_inner());
        let _wait = self
            .wake
            .wait_timeout_while(guard, Duration::from_millis(u64::from(timeout_ms)), |_| {
                !self.is_cancelled()
            })
            .unwrap_or_else(|err| err.into_inner());
        self.is_cancelled()
    }
}

/// The owner of the control state, kept alive for the whole request. Handing the
/// plugin a pointer to an `Arc`-backed allocation (rather than a stack address)
/// means the address stays valid even if the request task is moved by the
/// blocking pool before it calls the plugin.
struct ProviderControlOwner {
    state: Arc<ProviderControlState>,
}

/// The host's two plugin-facing control trampolines.
///
/// Both take the `user_data` the plugin got from
/// [`rpi_plugin_sdk::ProviderStreamControl`], which is **always** the live
/// `ProviderControlOwner` on the v2 path — so an early return for `user_data ==
/// null` is unreachable from a well-formed plugin and exists only as the
/// defensive floor against a forged pointer.
///
/// `is_cancelled`:
///
/// `chunk_kind` distinguishes content from structural chunks: a cancelled
/// request may still deliver structural frames that close the message the user
/// already saw, but has nobody left to receive model output.
extern "C" fn control_is_cancelled(user_data: *mut c_void, chunk_kind: u32) -> i32 {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if user_data.is_null() {
            // Should not happen: the host always passes a live owner. Report
            // "not cancelled" rather than aborting the plugin's request.
            tracing::error!("provider control callback missing its owner");
            return 0;
        }
        // SAFETY: the owner is heap-allocated and kept alive by the request's
        // `Arc<ProviderControlOwner>` for the whole plugin call.
        let owner = unsafe { &*(user_data as *const ProviderControlOwner) };
        if chunk_kind == rpi_plugin_sdk::ProviderChunkKind::Structural as u32 {
            return 0;
        }
        i32::from(owner.state.is_cancelled())
    }));
    outcome.unwrap_or(0)
}

/// `wait_cancelled_ms`: blocks up to `timeout_ms` on the plugin's own thread and
/// returns nonzero as soon as cancellation is observed.
extern "C" fn control_wait_cancelled_ms(user_data: *mut c_void, timeout_ms: u32) -> i32 {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if user_data.is_null() {
            tracing::error!("provider control callback missing its owner");
            return 0;
        }
        // SAFETY: as in `control_is_cancelled` — the owner outlives the call.
        let owner = unsafe { &*(user_data as *const ProviderControlOwner) };
        i32::from(owner.state.wait(timeout_ms))
    }));
    outcome.unwrap_or(0)
}

/// The callback state an emitted chunk is funneled through. Owned by the host,
/// pointed at by the plugin's `emit_user_data`, and — because it holds the only
/// sender of the chunk channel — the channel's lifetime marker: when the
/// blocking task drops it, the consumer's `recv()` returns `None`.
struct StreamEmitBridge {
    tx: queue::Sender,
    /// The plugin's own `plugin_free_string`: every emitted chunk arrives as a
    /// **plugin-owned** allocation, so only the plugin's deallocator may reclaim
    /// it (the host's would corrupt the plugin's heap).
    plugin_free_string: rpi_plugin_sdk::FreeStringFn,
    /// Latched cancellation shared with the plugin's control callbacks.
    control: Option<Arc<ProviderControlState>>,
}

/// One emitted chunk, already copied out of the plugin's allocation and safely
/// reclaimed through the plugin's `free_string`.
type StreamChunk = serde_json::Value;

/// The host's per-chunk callback. Invoked **synchronously inside the plugin's
/// request fn**, on the blocking-pool thread.
///
/// Ownership: `event_json` is plugin-produced, so it is reclaimed here with the
/// plugin's registered `plugin_free_string` — never with the host's allocator.
/// A missing context, a malformed envelope, or a panic in the host's own parsing
/// is reported as a nonzero rc; the plugin's string is still reclaimed exactly
/// once on every path, so a rejected chunk cannot leak.
extern "C" fn stream_emit_trampoline(event_json: StbString, user_data: *mut c_void) -> i32 {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if user_data.is_null() {
            // Invalid ABI invocation: there is no bridge to tell us which
            // deallocator owns this allocation. Never guess — logging is all we
            // can safely do without risking a cross-allocator free.
            tracing::error!("plugin provider emit callback missing its context");
            return 2;
        }
        // SAFETY: the blocking task owns this bridge for the entire plugin
        // request, including after the async driver returns on cancellation.
        let bridge = unsafe { &*(user_data as *const StreamEmitBridge) };
        // SAFETY: the plugin transfers a live, plugin-owned allocation exactly
        // once; `plugin_free_string` is that plugin's deallocator.
        let owned = unsafe { OwnedString::from_raw(event_json, Some(bridge.plugin_free_string)) };
        let text = owned.to_string_lossy();
        let bytes = text.len();
        let parsed = serde_json::from_str::<serde_json::Value>(&text);
        drop(text);
        drop(owned);
        match parsed {
            Ok(chunk) if chunk.is_object() => {
                // A cancelled run still needs the structural frames that close
                // the message it already showed, but has no consumer left for
                // model output. Rejecting content here is what tells a plugin to
                // stop producing instead of burning a full generation.
                let is_content = matches!(
                    chunk.get("type").and_then(serde_json::Value::as_str),
                    Some("text_delta") | Some("thinking_delta") | Some("toolcall_delta")
                );
                if bridge
                    .control
                    .as_ref()
                    .is_some_and(|state| state.is_cancelled())
                {
                    // After the aborted terminal, acknowledge cleanup frames
                    // without enqueueing events that no longer have a consumer.
                    return i32::from(is_content);
                }
                if bridge.tx.send(chunk, bytes).is_err() {
                    // Cancellation can also arrive during the capacity wait.
                    return if bridge
                        .control
                        .as_ref()
                        .is_some_and(|state| state.is_cancelled())
                    {
                        i32::from(is_content)
                    } else {
                        1
                    };
                }
                0
            }
            Ok(_) => {
                tracing::warn!("plugin provider emitted a non-object chunk — dropped");
                1
            }
            Err(error) => {
                tracing::warn!(%error, "plugin provider emitted unparseable JSON — dropped");
                1
            }
        }
    }));
    match outcome {
        Ok(rc) => rc,
        Err(_) => {
            tracing::error!("plugin provider emit callback panicked — chunk dropped");
            2
        }
    }
}

/// Run the streaming request fn inside `catch_unwind`, returning the plugin's
/// final message (or an error terminal). Mirrors
/// [`run_provider_request_inner`]'s contract: a null/empty `out`, a nonzero rc,
/// or an unparseable response all become an error terminal rather than a crash.
fn run_streaming_provider_request(
    snapshot: &Arc<RegistrySnapshot>,
    record: &RegisteredProvider,
    stream_fn: rpi_plugin_sdk::ProviderStreamRequestFn,
    bridge: &StreamEmitBridge,
    owner: &ProviderControlOwner,
    request_json: &str,
    model_id: &str,
) -> StreamingProviderOutcome {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !snapshot.is_active() {
            return Err("extensions provider registry is stale".to_string());
        }
        let req_ref = StbStringRef::from_str(request_json);
        let mut out = StbString::empty();
        let relay = bridge as *const StreamEmitBridge as *mut std::ffi::c_void;
        // `user_data` for the control callbacks is the owner's own address: the
        // trampolines cast it back, and the `Arc` keeps it alive for the call.
        let control = rpi_plugin_sdk::ProviderStreamControl {
            is_cancelled: control_is_cancelled,
            wait_cancelled_ms: control_wait_cancelled_ms,
            user_data: owner as *const ProviderControlOwner as *mut std::ffi::c_void,
        };
        let rc = stream_fn(
            req_ref,
            Some(stream_emit_trampoline),
            relay,
            &control as *const rpi_plugin_sdk::ProviderStreamControl,
            // No deferred-completion channel on this host: a plugin must finish
            // inside this call (see `ProviderStreamCompletionFn`).
            None,
            std::ptr::null_mut(),
            &mut out as *mut StbString,
            record.user_data,
        );
        // SAFETY: the callback transfers its live output to its registered
        // producer, on every rc path (including an empty slot).
        let out = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(record.plugin_free_string))
        };
        if rc != 0 {
            // A cancelled run makes the plugin return early; that is the
            // expected outcome, not a provider failure. The driver turns it into
            // an `Aborted` terminal, so it is reported as a cancellation here.
            if bridge
                .control
                .as_ref()
                .is_some_and(|state| state.is_cancelled())
            {
                return Err(CANCELLED_MARKER.to_string());
            }
            return Err(format!(
                "plugin provider {} streaming request returned error code {rc}",
                record.provider_id
            ));
        }
        let response_text = out.to_string_lossy();
        serde_json::from_str::<AssistantMessage>(&response_text).map_err(|err| {
            format!(
                "plugin provider {} returned unparseable response: {err}",
                record.provider_id
            )
        })
    }));

    match outcome {
        Ok(Ok(mut message)) => {
            if message.provider.is_empty() {
                message.provider = record.provider_id.clone();
            }
            if message.model.is_empty() {
                message.model = model_id.to_string();
            }
            message
        }
        Ok(Err(msg)) if msg == CANCELLED_MARKER => cancelled_message(record),
        Ok(Err(msg)) => error_message(record, &msg),
        Err(_) => {
            tracing::error!(
                "plugin provider {} panicked — refusing to unwind across FFI",
                record.provider_id
            );
            error_message(
                record,
                &format!(
                    "plugin provider {} panicked during request",
                    record.provider_id
                ),
            )
        }
    }
}

/// Sentinel the blocking request runner returns so the driver can tell "the run
/// was cancelled" from "the plugin failed". It cannot collide with a real plugin
/// error: plugin-side failures are formatted as `plugin provider {id} …`, and
/// this marker starts with a NUL no plugin-produced text goes through.
const CANCELLED_MARKER: &str = "\u{0}rpi:cancelled";

/// The terminal message for a request whose caller went away. `StopReason::Aborted`
/// is what the built-in providers use, and the crate's `push_terminal` maps it to
/// `AssistantMessageEvent::Error { reason: Aborted }` — so a plugin provider is
/// indistinguishable from a built-in one in the aborted case.
fn cancelled_message(record: &RegisteredProvider) -> AssistantMessage {
    AssistantMessage::terminal(
        api_from_style(&record.api_style),
        record.provider_id.clone(),
        String::new(),
        rpi_ai::types::StopReason::Aborted,
        "plugin provider request was cancelled",
        0,
    )
}

/// An error terminal for a plugin provider. The api string is decoded exactly
/// like a deserialized `Api` (an unknown style becomes `Api::Other`), so an error
/// message from a plugin provider carries the same `api` a success would.
fn error_message(record: &RegisteredProvider, message: &str) -> AssistantMessage {
    AssistantMessage::terminal(
        api_from_style(&record.api_style),
        record.provider_id.clone(),
        String::new(),
        rpi_ai::types::StopReason::Error,
        message.to_string(),
        0,
    )
}

/// Decode the registration's `api_style` into an [`Api`](rpi_ai::types::Api).
/// Mirrors `Api`'s own `Deserialize` (including the `Other(..)` fallback) without
/// needing a throwaway serde pass.
fn api_from_style(style: &str) -> rpi_ai::types::Api {
    serde_json::from_value::<rpi_ai::types::Api>(serde_json::Value::String(style.to_string()))
        .unwrap_or_else(|_| rpi_ai::types::Api::Other(style.to_string()))
}

/// Build the `{ model, context, options }` request envelope shared by the
/// one-shot and streaming provider paths.
fn request_envelope_json(model: &Model, ctx: &Context, opts: &SimpleStreamOptions) -> String {
    serde_json::json!({
        "model": serde_json::to_value(model).unwrap_or(serde_json::Value::Null),
        "context": serde_json::to_value(ctx).unwrap_or(serde_json::Value::Null),
        "options": options_json(opts),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtensionRegistry;

    #[test]
    fn stream_idle_duration_preserves_disabled_default_and_large_values_across_abi() {
        for (duration, expected) in [
            (None, serde_json::Value::Null),
            (Some(std::time::Duration::ZERO), serde_json::json!(0)),
            (
                Some(std::time::Duration::from_secs(45)),
                serde_json::json!(45_000),
            ),
            // A valid u64 CLI seconds value must not wrap to a tiny timeout.
            (
                Some(std::time::Duration::from_secs(u64::MAX / 1000 + 1)),
                serde_json::json!(u64::MAX),
            ),
        ] {
            let options = SimpleStreamOptions {
                stream_idle_timeout: duration,
                ..Default::default()
            };
            assert_eq!(options_json(&options)["streamIdleTimeoutMs"], expected);
        }
    }

    rpi_plugin_sdk::string_handler! {
        /// A stub `ProviderRequestFn` that writes a minimal assistant-message JSON
        /// into `out`. Mirrors the shape a real plugin's request fn returns.
        fn ok_request_fn(_req, out, _ud) {
        let json = r#"{"role":"assistant","content":[{"type":"text","text":"hi"}],"api":"faux","provider":"pluggy","model":"","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":0}"#;
        i32::from(!out.write(json.to_string()))
    } }

    /// A free fn matching the stub's allocation (`StbString::from_string` ⇒ a
    /// `Box<[u8]>` reclaimed by reconstructing the slice).
    use rpi_plugin_sdk::callbacks::free_string as stub_free;

    /// A v2 "provider" whose synchronous request body blocks the way a real one
    /// would (it waits for cancellation after an initial burst) and reports what
    /// it saw. Used to drive the whole cancellation path in-process, without the
    /// cdylib, so the decision logic is covered by a fast unit test too.
    mod cancel_provider {
        use super::*;
        use rpi_plugin_sdk::{ProviderStreamCompletionFn, ProviderStreamControl};
        use std::sync::atomic::AtomicUsize;

        pub static EMITTED: AtomicUsize = AtomicUsize::new(0);
        pub static OBSERVED: AtomicUsize = AtomicUsize::new(0);

        pub extern "C" fn request_v2(
            _req: StbStringRef,
            emit: Option<rpi_plugin_sdk::ProviderStreamEmitFn>,
            emit_user_data: *mut c_void,
            control: *const ProviderStreamControl,
            _completion: Option<ProviderStreamCompletionFn>,
            _completion_user_data: *mut c_void,
            out: *mut StbString,
            _ud: *mut c_void,
        ) -> i32 {
            let control = unsafe { *control };
            if let Some(emit) = emit {
                let start = r#"{"type":"text_start","contentIndex":0}"#;
                let _ = emit(StbString::from_string(start.to_string()), emit_user_data);
                for i in 0..50 {
                    // Block on the host's control surface, exactly as a provider
                    // parked in an SSE read would.
                    if (control.wait_cancelled_ms)(control.user_data, 20) != 0 {
                        OBSERVED.fetch_add(1, Ordering::SeqCst);
                        return 1;
                    }
                    let chunk =
                        format!(r#"{{"type":"text_delta","contentIndex":0,"delta":"{i},"}}"#);
                    if emit(StbString::from_string(chunk), emit_user_data) != 0 {
                        OBSERVED.fetch_add(1, Ordering::SeqCst);
                        EMITTED.fetch_add(1, Ordering::SeqCst);
                        return 1;
                    }
                    EMITTED.fetch_add(1, Ordering::SeqCst);
                }
            }
            let json = r#"{"role":"assistant","content":[{"type":"text","text":"completed"}],"api":"faux","provider":"cancel-probe","model":"m","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":0}"#;
            unsafe {
                *out = StbString::from_string(json.to_string());
            }
            0
        }
    }

    /// `run_provider_request_inner` round-trips a nonzero-rc plugin response to
    /// `Err`, and a valid-message response to `Ok` (with provider stamped from
    /// the record when the plugin omits it).
    #[test]
    fn request_fn_round_trip_ok_and_err() {
        let mut registry = ExtensionRegistry::new();
        registry.register_provider(RegisteredProvider {
            action_bridge: None,
            provider_id: "pluggy".to_string(),
            base_url: "https://example".to_string(),
            api_style: "anthropic-messages".to_string(),
            request_fn: ok_request_fn,
            stream_fn: None,
            plugin_free_string: stub_free,
            user_data: std::ptr::null_mut(),
        });
        let snap = Arc::new(registry.snapshot());
        let record = snap.providers()[0].clone();

        // Valid response → Ok(message).
        let outcome = run_provider_request(&record, r#"{"model":"m"}"#, "m");
        match outcome {
            ProviderOutcome::Ok(m) => {
                assert_eq!(m.provider, "pluggy");
                assert_eq!(m.model, "m");
                assert_eq!(m.stop_reason, rpi_ai::types::StopReason::Stop);
                assert_eq!(m.content.len(), 1);
            }
            _ => panic!("expected Ok"),
        }

        // A request fn that returns nonzero rc ⇒ Err (registered separately).
        extern "C" fn err_request_fn(
            _req: StbStringRef,
            _out: *mut StbString,
            _ud: *mut std::ffi::c_void,
        ) -> i32 {
            7
        }
        let err_record = RegisteredProvider {
            action_bridge: None,
            provider_id: "pluggy".to_string(),
            base_url: "https://example".to_string(),
            api_style: "anthropic-messages".to_string(),
            request_fn: err_request_fn,
            stream_fn: None,
            plugin_free_string: stub_free,
            user_data: std::ptr::null_mut(),
        };
        let outcome = run_provider_request(&err_record, r#"{"model":"m"}"#, "m");
        assert!(matches!(outcome, ProviderOutcome::Err(_)));
    }

    /// A streaming provider's deltas must reach the event stream **in order**, as
    /// `TextStart` → `TextDelta`×N → `TextEnd`, all before the terminal `Done`.
    /// This is the property the whole feature exists for: the consumer renders
    /// while the plugin is still producing.
    #[test]
    fn stream_chunks_map_to_ordered_events() {
        let mut mapper = StreamMapper::new(rpi_ai::types::Api::Faux, "pluggy".into(), "m".into());
        let chunks = [
            serde_json::json!({"type":"start"}),
            serde_json::json!({"type":"text_start","contentIndex":0}),
            serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"hel"}),
            serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"lo"}),
            serde_json::json!({"type":"text_end","contentIndex":0}),
        ];
        let mut tags = Vec::new();
        for chunk in chunks {
            if let Some(event) = mapper.apply(chunk) {
                tags.push(event.type_tag());
            }
            while let Some(deferred) = mapper.take_deferred() {
                if let Some(event) = mapper.apply(deferred) {
                    tags.push(event.type_tag());
                }
            }
        }
        assert_eq!(
            tags,
            vec![
                "start",
                "text_start",
                "text_delta",
                "text_delta",
                "text_end"
            ],
            "chunks must map to an ordered, non-terminal event sequence"
        );
        assert_eq!(
            mapper.accumulated().content[0],
            rpi_ai::types::Content::text("hello"),
            "deltas accumulate into the block they name"
        );
    }

    /// The aborted terminal must map onto `ErrorReason::Aborted`, not `Error`:
    /// a user-cancelled run is not a provider fault, and the agent loop and TUI
    /// report the two differently.
    #[test]
    fn an_aborted_message_terminates_with_aborted_not_error() {
        let (mut prod, mut stream) = create_assistant_message_event_stream();
        let record = RegisteredProvider {
            action_bridge: None,
            provider_id: "pluggy".to_string(),
            base_url: "https://example".to_string(),
            api_style: "faux".to_string(),
            request_fn: ok_request_fn,
            stream_fn: None,
            plugin_free_string: stub_free,
            user_data: std::ptr::null_mut(),
        };
        futures::executor::block_on(push_terminal(&mut prod, cancelled_message(&record)));
        let event = futures::executor::block_on(stream.next()).expect("a terminal event");
        match event {
            AssistantMessageEvent::Error { reason, error } => {
                assert_eq!(reason, ErrorReason::Aborted);
                assert_eq!(error.stop_reason, rpi_ai::types::StopReason::Aborted);
            }
            other => panic!("expected Aborted Error, got {other:?}"),
        }
    }

    /// A cancelled v2 request terminates on `Aborted` promptly, and the plugin's
    /// control surface reports it — the end-to-end property the cancel protocol
    /// exists for, driven in-process rather than through the cdylib.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_v2_request_terminates_aborted_and_tells_the_plugin() {
        cancel_provider::EMITTED.store(0, Ordering::SeqCst);
        cancel_provider::OBSERVED.store(0, Ordering::SeqCst);

        let record = RegisteredProvider {
            action_bridge: None,
            provider_id: "cancel-probe".to_string(),
            base_url: "https://example".to_string(),
            api_style: "faux".to_string(),
            request_fn: ok_request_fn,
            stream_fn: Some(cancel_provider::request_v2),
            plugin_free_string: stub_free,
            user_data: std::ptr::null_mut(),
        };
        let mut registry = ExtensionRegistry::new();
        registry.register_streaming_provider(record.clone());
        let snapshot = Arc::new(registry.snapshot());

        let model = Model::new(
            "m",
            "M",
            rpi_ai::types::Api::Faux,
            "cancel-probe",
            "https://example",
        );
        let ctx = Context::new(Vec::new());
        let signal = tokio_util::sync::CancellationToken::new();
        let opts = SimpleStreamOptions {
            signal: signal.clone(),
            ..SimpleStreamOptions::default()
        };

        let (mut prod, mut stream) = create_assistant_message_event_stream();
        // The driver returns the terminal message; the derived event lands on the
        // producer, so read the stream for the events the consumer would see.
        tokio::spawn(async move {
            let stream_fn = record.stream_fn.expect("a streaming fn");
            let message = drive_streaming_plugin_provider(
                &snapshot,
                &record,
                stream_fn,
                Handle::current(),
                ProviderRequestLifetime::new(PluginKeepalive::empty()),
                &model,
                &ctx,
                &opts,
                &mut prod,
            )
            .await;
            push_terminal(&mut prod, message).await;
        });

        let mut aborted = false;
        let mut saw_delta = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while let Some(event) = stream.next().await {
            match &event {
                AssistantMessageEvent::TextDelta { .. } if !saw_delta => {
                    saw_delta = true;
                    signal.cancel();
                }
                AssistantMessageEvent::Error { reason, .. } => {
                    aborted = *reason == ErrorReason::Aborted;
                }
                _ => {}
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a cancelled request must terminate promptly"
            );
        }

        assert!(saw_delta, "the provider must have started producing");
        assert!(aborted, "the terminal must be Aborted");
        // The host terminal no longer waits for the plugin to unwind.
        tokio::time::timeout(Duration::from_secs(2), async {
            while cancel_provider::OBSERVED.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the plugin observes cancellation eventually");
        assert_eq!(
            cancel_provider::OBSERVED.load(Ordering::SeqCst),
            1,
            "the plugin must have observed the cancellation through its control surface"
        );
        assert!(
            cancel_provider::EMITTED.load(Ordering::SeqCst) < 50,
            "the plugin must stop before its 50-delta budget"
        );
    }

    /// A plugin that ignores the control surface still terminates correctly: the
    /// host stops forwarding content chunks once cancelled, so `emit` reports
    /// rejection and the run ends on `Aborted` rather than hanging.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_request_stops_forwarding_content_even_if_the_plugin_pushes() {
        let owner = Arc::new(ProviderControlOwner {
            state: Arc::new(ProviderControlState::default()),
        });
        let (tx, mut rx) = queue::channel(64, 4 * 1024 * 1024, Arc::clone(&owner.state));
        let bridge = StreamEmitBridge {
            tx,
            plugin_free_string: stub_free,
            control: Some(Arc::clone(&owner.state)),
        };
        let relay = &bridge as *const StreamEmitBridge as *mut c_void;

        // Not yet cancelled: a content chunk is forwarded.
        let delta = r#"{"type":"text_delta","contentIndex":0,"delta":"a"}"#;
        assert_eq!(
            stream_emit_trampoline(StbString::from_string(delta.to_string()), relay),
            0
        );
        assert!(rx.try_recv().is_ok(), "an un-cancelled chunk is delivered");

        owner.state.cancel();

        // Cancelled: the same chunk is rejected, and a structural one still lands
        // so the message can be closed.
        assert_eq!(
            stream_emit_trampoline(StbString::from_string(delta.to_string()), relay),
            1,
            "a content chunk is refused once the run is cancelled"
        );
        let end = r#"{"type":"text_end","contentIndex":0,"content":"a"}"#;
        assert_eq!(
            stream_emit_trampoline(StbString::from_string(end.to_string()), relay),
            0,
            "a structural chunk stays admissible while winding down"
        );
        assert!(
            rx.try_recv().is_err(),
            "cleanup must not produce post-terminal events"
        );
    }

    /// The control surface a v2 plugin polls: `is_cancelled` reports the latched
    /// flag for content chunks but never for structural ones (a cancelled run
    /// still needs the frames that close the message it already showed), and
    /// `wait_cancelled_ms` returns promptly once the flag is set.
    #[test]
    fn the_control_surface_reports_cancellation_for_content_only() {
        let owner = Arc::new(ProviderControlOwner {
            state: Arc::new(ProviderControlState::default()),
        });
        let user_data = Arc::as_ptr(&owner) as *mut c_void;
        let content = rpi_plugin_sdk::ProviderChunkKind::Content as u32;
        let structural = rpi_plugin_sdk::ProviderChunkKind::Structural as u32;

        assert_eq!(control_is_cancelled(user_data, content), 0);
        assert_eq!(control_wait_cancelled_ms(user_data, 0), 0);

        owner.state.cancel();

        assert_eq!(
            control_is_cancelled(user_data, content),
            1,
            "content chunks must report cancelled"
        );
        assert_eq!(
            control_is_cancelled(user_data, structural),
            0,
            "structural chunks stay admissible while a cancelled request winds down"
        );
        // A blocking wait returns as soon as the (already-set) flag is seen.
        let started = std::time::Instant::now();
        assert_eq!(control_wait_cancelled_ms(user_data, 60_000), 1);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "an already-cancelled wait must not sleep out its timeout"
        );
        // A null `user_data` is the defensive floor: never report cancelled, so a
        // plugin keeps its normal behavior instead of aborting.
        assert_eq!(control_is_cancelled(std::ptr::null_mut(), content), 0);
        assert_eq!(control_wait_cancelled_ms(std::ptr::null_mut(), 0), 0);
    }

    /// A plugin that emits a delta with no preceding `start` still gets a
    /// well-formed stream: the host inserts `Start` first and re-applies the
    /// delta, so the frame encoder's `before start` rule is never violated.
    #[test]
    fn a_delta_before_start_still_opens_the_stream() {
        let mut mapper = StreamMapper::new(rpi_ai::types::Api::Faux, "pluggy".into(), "m".into());
        let first = mapper
            .apply(serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"x"}))
            .expect("a start event");
        assert_eq!(first.type_tag(), "start");
        let deferred = mapper.take_deferred().expect("the parked delta");
        let second = mapper.apply(deferred).expect("the delta event");
        assert_eq!(second.type_tag(), "text_delta");
    }

    /// Tool-call deltas accumulate raw JSON and the end event carries the parsed
    /// object — the same `partial_json` invariant the Anthropic mapper holds, so
    /// a `ToolCallEnd` never contains unparseable arguments.
    #[test]
    fn tool_call_deltas_accumulate_json() {
        let mut mapper = StreamMapper::new(rpi_ai::types::Api::Faux, "pluggy".into(), "m".into());
        let mut tags = Vec::new();
        for chunk in [
            serde_json::json!({"type":"toolcall_start","contentIndex":0,"id":"c1","name":"echo"}),
            serde_json::json!({"type":"toolcall_delta","contentIndex":0,"delta":"{\"text\":"}),
            serde_json::json!({"type":"toolcall_delta","contentIndex":0,"delta":"\"hi\"}"}),
        ] {
            if let Some(event) = mapper.apply(chunk) {
                tags.push(event.type_tag());
            }
            while let Some(deferred) = mapper.take_deferred() {
                if let Some(event) = mapper.apply(deferred) {
                    tags.push(event.type_tag());
                }
            }
        }
        // The first `toolcall_start` arrives before the stream is open, so the
        // host's `start` goes out first and the chunk is re-applied right after.
        assert_eq!(
            tags,
            vec![
                "start",
                "toolcall_start",
                "toolcall_delta",
                "toolcall_delta"
            ]
        );
        let event = mapper
            .apply(
                serde_json::json!({"type":"toolcall_end","contentIndex":0,"id":"c1","name":"echo"}),
            )
            .expect("tool call end");
        let AssistantMessageEvent::ToolCallEnd { tool_call, .. } = event else {
            panic!("expected ToolCallEnd");
        };
        assert_eq!(tool_call.name, "echo");
        assert_eq!(tool_call.arguments["text"], "hi");
    }
}
