//! Durable assistant-message progress: record every streamed frame, and salvage
//! what committed if the run dies.
//!
//! Ports the *write* and *recovery* halves of upstream's frame progress
//! (`harness/runtime/progress.ts::openFrameProgress` +
//! `harness/runtime/drive/recovery.ts::recoverAssistantGeneration`).
//!
//! # Storage choice
//!
//! Native keeps frames in session-scoped **list** storage
//! (`pendingAssistantFrames(operationId, responseEntryId)`), which is separate
//! from the branch. rpi has no list primitive, so frames ride the **record
//! stream** (`LaneRecord::AssistantFrame`) instead.
//!
//! That choice is load-bearing: records are not entries, so a frame can never
//! enter the branch path the model's context is built from. Storing frames as
//! custom *entries* (the obvious first attempt) inflates the transcript — one
//! entry per frame — and the harness's own entry-count tests catch it.
//!
//! # Model
//!
//! - Each streamed assistant message gets a `stream_index` within its run.
//! - Every frame is appended as one `AssistantFrame { op: Append }` record.
//! - When the run's messages are persisted, one `ClearRun` record retires the
//!   whole run's frames: they were progress, not history.
//! - If the run dies first, [`salvage_run_frames`] replays the committed prefix
//!   into the partial message and marks it interrupted, so the content the model
//!   had already produced is kept instead of vanishing.
//!
//! Before this, a run persisted nothing until it finished, so a crash after 14
//! minutes of work left only the user's prompt. See
//! `docs/debugging/llm-repetition-forensics.md` §十一.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use rpi_agent::{AgentEmitter, AgentEvent, AgentMessage};
use rpi_ai::frames::{reduce_frames, AssistantMessageFrame, AssistantMessageFrameEncoder};
use rpi_ai::types::{AssistantMessage, AssistantMessageEvent, StopReason, Usage};

use crate::result::{HarnessError, HarnessResult};
use crate::session::session::Session;
use crate::session::types::{
    AssistantFrameOp, AssistantFrameRecord, Entry, EntryOrder, EntryQuery, LaneRecord, RecordBase,
    RecordQuery,
};

/// The `record_type` tag for [`AssistantFrameRecord`].
pub const ASSISTANT_FRAME_RECORD_TYPE: &str = "assistant_frame";

/// The text upstream attaches to a message reconstructed from frames. Kept
/// verbatim so a reader (or a tool) can recognise the case.
pub const INTERRUPTED_NOTICE: &str = "Assistant request was interrupted. The preceding content is \
the latest committed partial; newer live output may be missing and the external outcome is unknown.";

#[derive(Clone, Default)]
pub(crate) struct VolatileStream {
    frames: Vec<AssistantMessageFrame>,
    committed_entries: Vec<String>,
}
#[derive(Default)]
pub(crate) struct VolatileProgress {
    runs: BTreeMap<String, BTreeMap<usize, VolatileStream>>,
}
pub(crate) type SharedVolatileProgress = Arc<Mutex<VolatileProgress>>;

/// Turn a replayed partial into the interrupted message rpi records.
///
/// Mirrors `interruptedAssistantMessage`: `stopReason: error` plus the notice,
/// and **usage zeroed** so a salvaged partial cannot double-count tokens that a
/// later retry bills again.
pub fn interrupted_message(partial: AssistantMessage) -> AssistantMessage {
    AssistantMessage {
        stop_reason: StopReason::Error,
        error_message: Some(INTERRUPTED_NOTICE.to_string()),
        usage: Usage::zero(),
        ..partial
    }
}

/// Text attached to the synthetic tool result that stands in for a tool call
/// whose real result never landed. Mirrors the intent of upstream's
/// interrupted-operation notices: say plainly that the outcome is unknown, so
/// neither the user nor the model assumes the tool had no effect.
pub const INTERRUPTED_TOOL_RESULT: &str = "Tool execution was interrupted before its result was \
recorded. The external outcome is unknown: this tool may or may not have taken effect.";

/// A tool call whose result never landed, from the committed frames of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownToolOutcome {
    pub tool_call_id: String,
    pub tool_name: String,
    /// The arguments the model asked for, recovered from the frames. This is what
    /// lets a reader judge what the tool *would* have done.
    pub arguments: serde_json::Value,
    pub recorded: Option<serde_json::Value>,
}

/// The tool calls of `partial` whose results are not committed on `lane`.
///
/// Returns a `Result` rather than panicking so a recovery path can degrade to
/// "no report" instead of failing the whole recovery.
///
/// Resolved by `tool_call_id` against the lane's entries rather than by a
/// reserved result-entry id: rpi persists tool results with a freshly minted id
/// at run end, so the id a `tool_started` record would have named is not known
/// when the tool actually runs. `tool_call_id` is the durable link that exists.
pub async fn unknown_tool_outcomes(
    session: &Session,
    lane: &str,
    partial: &AssistantMessage,
) -> HarnessResult<Vec<UnknownToolOutcome>> {
    let calls: Vec<&rpi_ai::types::ToolCall> = partial
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::types::Content::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    if calls.is_empty() {
        return Ok(Vec::new());
    }

    let entries = session
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .map_err(|error| HarnessError::io(error.to_string()))?;
    let landed: std::collections::BTreeSet<&str> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Message(message) => match &message.message {
                AgentMessage::ToolResult(result) => Some(result.tool_call_id.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let _ = lane; // entries are session-wide; kept for call-site clarity.

    Ok(calls
        .into_iter()
        .filter(|call| !landed.contains(call.id.as_str()))
        .map(|call| UnknownToolOutcome {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
            recorded: call.recorded.clone(),
        })
        .collect())
}

/// The synthetic error result that stands in for an interrupted tool call.
pub fn interrupted_tool_result(outcome: &UnknownToolOutcome, timestamp: i64) -> AgentMessage {
    AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
        structured_content: None,
        role: rpi_ai::types::ToolResultRole,
        tool_call_id: outcome.tool_call_id.clone(),
        tool_name: outcome.tool_name.clone(),
        content: vec![rpi_ai::types::Content::text(INTERRUPTED_TOOL_RESULT)],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        is_error: true,
        timestamp,
    }))
}

/// Reduce the committed frames of every stream in `run_id`, in stream order.
///
/// A run that was cleared (i.e. it committed) yields nothing. Unreadable frame
/// sequences are skipped rather than failing the caller — recovery must never
/// make a session unreadable.
pub(crate) async fn replay_run_frames(
    session: &Session,
    run_id: &str,
) -> HarnessResult<Vec<AssistantMessage>> {
    if let Some(progress) = session.volatile_progress() {
        // Copy only on recovery. The streaming path moves typed frames into the
        // cache, retaining the same encoder/reducer semantics as durable mode.
        let streams = progress
            .lock()
            .unwrap()
            .runs
            .get(run_id)
            .cloned()
            .unwrap_or_default();
        let mut replayed = Vec::new();
        for stream in streams.into_values() {
            let mut committed = false;
            for entry_id in &stream.committed_entries {
                if session
                    .get_entry(entry_id)
                    .await
                    .map_err(|error| HarnessError::io(error.to_string()))?
                    .is_some()
                {
                    committed = true;
                    break;
                }
            }
            if !committed {
                match reduce_frames(&stream.frames) {
                    Ok(Some(partial)) => replayed.push(partial),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "discarding an unreadable in-memory assistant frame stream")
                    }
                }
            }
        }
        return Ok(replayed);
    }
    let records = session
        .find_records(&RecordQuery {
            record_type: Some(ASSISTANT_FRAME_RECORD_TYPE),
            run_id: Some(run_id.to_string()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .map_err(|error| HarnessError::io(error.to_string()))?;

    let mut streams: BTreeMap<usize, Vec<AssistantMessageFrame>> = BTreeMap::new();
    // Streams whose message is already an entry (committed mid-run by
    // commit-on-settle). Replaying one would duplicate it in the transcript.
    let mut cleared: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut commits: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for record in records {
        let LaneRecord::AssistantFrame(frame) = record else {
            continue;
        };
        match frame.op {
            // The run committed after this point, so its frames were only
            // progress and nothing needs salvaging.
            AssistantFrameOp::ClearRun => return Ok(Vec::new()),
            AssistantFrameOp::ClearStream => {
                cleared.insert(frame.stream_index);
            }
            AssistantFrameOp::CommitStream => {
                if let Some(entry_id) = frame
                    .frame
                    .as_ref()
                    .and_then(|value| value.get("entryId"))
                    .and_then(|value| value.as_str())
                {
                    commits
                        .entry(frame.stream_index)
                        .or_default()
                        .push(entry_id.to_string());
                }
            }
            AssistantFrameOp::Append => {
                let Some(value) = frame.frame else {
                    continue;
                };
                match serde_json::from_value::<AssistantMessageFrame>(value) {
                    Ok(decoded) => streams.entry(frame.stream_index).or_default().push(decoded),
                    Err(error) => tracing::warn!(
                        stream_index = frame.stream_index,
                        %error,
                        "skipping an undecodable assistant frame during recovery"
                    ),
                }
            }
        }
    }

    let mut replayed = Vec::new();
    for (index, frames) in streams {
        if cleared.contains(&index) {
            continue;
        }
        let mut committed = false;
        if let Some(entry_ids) = commits.get(&index) {
            for entry_id in entry_ids {
                if session
                    .get_entry(entry_id)
                    .await
                    .map_err(|error| HarnessError::io(error.to_string()))?
                    .is_some()
                {
                    committed = true;
                    break;
                }
            }
        }
        if committed {
            continue;
        }
        match reduce_frames(&frames) {
            Ok(Some(partial)) => replayed.push(partial),
            Ok(None) => {}
            Err(error) => tracing::warn!(
                %error,
                "discarding an unreadable assistant frame stream during recovery"
            ),
        }
    }
    Ok(replayed)
}

/// Record the entry a stream will commit under before appending that entry.
/// Unlike a clear marker, this preserves progress if the commit never lands.
pub(crate) async fn bind_stream_entry(
    session: &Session,
    lane: &str,
    run_id: &str,
    stream_index: usize,
    entry_id: &str,
) -> HarnessResult<()> {
    if let Some(progress) = session.volatile_progress() {
        progress
            .lock()
            .unwrap()
            .runs
            .entry(run_id.to_owned())
            .or_default()
            .entry(stream_index)
            .or_default()
            .committed_entries
            .push(entry_id.to_owned());
        return Ok(());
    }
    session
        .append_record(LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: session.id_generator().next(),
                seq: 0,
                lane: lane.to_string(),
                timestamp: 0,
            },
            run_id: run_id.to_string(),
            stream_index,
            op: AssistantFrameOp::CommitStream,
            frame: Some(serde_json::json!({"entryId": entry_id})),
        }))
        .await
        .map_err(|error| HarnessError::io(error.to_string()))?;
    Ok(())
}

/// Replay the committed frames of every stream in `run_id`.
///
/// Returns the salvaged messages in stream order. A run that was cleared (i.e.
/// it committed) yields nothing.
pub async fn salvage_run_frames(
    session: &Session,
    run_id: &str,
) -> HarnessResult<Vec<AssistantMessage>> {
    Ok(replay_run_frames(session, run_id)
        .await?
        .into_iter()
        .map(interrupted_message)
        .collect())
}

/// Emitter wrapper that forwards every event and durably records assistant
/// frames as they stream.
///
/// The recording state is shared behind an `Arc` so `emit` can move its clones
/// into the returned future.
pub struct FrameRecordingEmitter {
    inner: Arc<dyn AgentEmitter>,
    shared: Arc<RecorderShared>,
}

struct RecorderShared {
    session: Session,
    /// The lane the frames belong to. Required: the store validates the lane and
    /// rejects a record with an unknown one.
    lane: String,
    run_id: String,
    state: Mutex<RecorderState>,
    volatile: Option<SharedVolatileProgress>,
}

struct RecorderState {
    next_index: usize,
    /// The stream currently being recorded: its index and encoder.
    current: Option<(usize, AssistantMessageFrameEncoder)>,
}

impl RecorderShared {
    /// Append one `AssistantFrame` record.
    async fn append_frame_record(
        &self,
        stream_index: usize,
        op: AssistantFrameOp,
        frame: Option<serde_json::Value>,
    ) -> HarnessResult<()> {
        let record = LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: self.session.id_generator().next(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            run_id: self.run_id.clone(),
            stream_index,
            op,
            frame,
        });
        self.session
            .append_record(record)
            .await
            .map_err(|error| HarnessError::io(error.to_string()))?;
        Ok(())
    }

    /// Advance the recorder for one event and return the frame to append, if
    /// any. Pure bookkeeping — no I/O.
    fn take_frame(&self, event: &AssistantMessageEvent) -> Option<(usize, AssistantMessageFrame)> {
        let mut state = self.state.lock().unwrap();
        let terminal = matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        );
        if matches!(event, AssistantMessageEvent::Start { .. }) {
            if let Some((previous, _)) = state.current.take() {
                tracing::warn!(
                    stream_index = previous,
                    "assistant frame stream restarted without a terminal event"
                );
            }
            let index = state.next_index;
            state.next_index += 1;
            state.current = Some((index, AssistantMessageFrameEncoder::new()));
        }
        let (index, encoder) = state.current.as_mut()?;
        let index = *index;
        let frame = match encoder.encode(event) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!(
                    stream_index = index,
                    %error,
                    "dropping a frame that violates the stream contract"
                );
                None
            }
        };
        if terminal {
            state.current = None;
        }
        frame.map(|frame| (index, frame))
    }
}

impl FrameRecordingEmitter {
    pub fn new(
        inner: Arc<dyn AgentEmitter>,
        session: Session,
        lane: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Self {
        let volatile = session.volatile_progress();
        Self {
            inner,
            shared: Arc::new(RecorderShared {
                session,
                lane: lane.into(),
                run_id: run_id.into(),
                volatile,
                state: Mutex::new(RecorderState {
                    next_index: 0,
                    current: None,
                }),
            }),
        }
    }

    /// The run this recorder is following.
    pub fn run_id(&self) -> &str {
        &self.shared.run_id
    }

    /// Retire this run's frames — called once its messages are persisted.
    pub async fn clear(&self) -> HarnessResult<()> {
        if let Some(progress) = &self.shared.volatile {
            progress.lock().unwrap().runs.remove(&self.shared.run_id);
            return Ok(());
        }
        self.shared
            .append_frame_record(0, AssistantFrameOp::ClearRun, None)
            .await
    }

    /// Retire one stream, whose message has just been committed as an entry.
    ///
    /// Without this, salvage would replay it again on a crash and duplicate the
    /// message: the run-wide `ClearRun` marker only arrives at the end of the run.
    pub async fn clear_stream(&self, stream_index: usize) -> HarnessResult<()> {
        if let Some(progress) = &self.shared.volatile {
            if let Some(run) = progress.lock().unwrap().runs.get_mut(&self.shared.run_id) {
                run.remove(&stream_index);
            }
            return Ok(());
        }
        self.shared
            .append_frame_record(stream_index, AssistantFrameOp::ClearStream, None)
            .await
    }

    /// Record one event's frame durably. Failures are logged, never fatal: a
    /// session that cannot record progress must still run the agent.
    async fn record(shared: &RecorderShared, event: &AssistantMessageEvent) {
        let Some((index, frame)) = shared.take_frame(event) else {
            return;
        };
        if let Some(progress) = &shared.volatile {
            progress
                .lock()
                .unwrap()
                .runs
                .entry(shared.run_id.clone())
                .or_default()
                .entry(index)
                .or_default()
                .frames
                .push(frame);
            return;
        }
        let value = match serde_json::to_value(&frame) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "could not serialize an assistant frame");
                return;
            }
        };
        if let Err(error) = shared
            .append_frame_record(index, AssistantFrameOp::Append, Some(value))
            .await
        {
            tracing::warn!(stream_index = index, %error, "could not append an assistant frame");
        }
    }
}

impl AgentEmitter for FrameRecordingEmitter {
    fn prepare_event(&self, event: AgentEvent) -> BoxFuture<'static, AgentEvent> {
        self.inner.prepare_event(event)
    }

    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        self.emit_inner(event, false)
    }
    fn emit_prepared(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        self.emit_inner(event, true)
    }

    fn try_emit(&self, event: AgentEvent) {
        self.inner.try_emit(event);
    }
}

impl FrameRecordingEmitter {
    fn emit_inner(&self, event: AgentEvent, prepared: bool) -> BoxFuture<'static, ()> {
        let inner = Arc::clone(&self.inner);
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            // Borrow from the owned event while recording, then forward it.
            // This avoids cloning every delta string into a temporary event.
            match &event {
                AgentEvent::MessageStart {
                    message: AgentMessage::Assistant(assistant),
                } => {
                    let start = AssistantMessageEvent::Start {
                        partial: Arc::new((**assistant).clone()),
                    };
                    FrameRecordingEmitter::record(&shared, &start).await;
                }
                AgentEvent::MessageUpdate {
                    assistant_message_event,
                    ..
                } => {
                    FrameRecordingEmitter::record(&shared, assistant_message_event).await;
                }
                AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(_),
                } => {
                    shared.state.lock().unwrap().current = None;
                }
                _ => {}
            }
            if prepared {
                inner.emit_prepared(event).await;
            } else {
                inner.emit(event).await;
            }
        })
    }
}

#[cfg(test)]
mod volatile_tests {
    use super::*;
    use crate::session::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
    use crate::session::types::{ProvisionedEntry, ProvisionedKind, SessionMetadata};
    use rpi_agent::CollectorEmitter;
    use rpi_ai::{Api, Content};

    fn session(volatile: bool) -> Session {
        let session = Session::new(
            Arc::new(InMemorySessionStorage::new(
                SessionMetadata {
                    id: "memory".into(),
                    created_at: 0,
                    parent_session_id: None,
                },
                Arc::new(FakeClock::new()),
                Arc::new(CounterIdGenerator::new()),
            )),
            None,
        );
        if volatile {
            session.with_volatile_progress()
        } else {
            session
        }
    }
    fn events() -> Vec<AgentEvent> {
        let mut message = AssistantMessage::empty(Api::Faux, "faux", "m", 42);
        let mut events = vec![AgentEvent::MessageStart {
            message: message.clone().into(),
        }];
        // Start snapshots may already cover a delta not yet observed by the
        // encoder. Both modes must retain the encoder's catch-up semantics.
        message.content.push(Content::text("ahead"));
        let partial = Arc::new(message.clone());
        events.push(AgentEvent::MessageUpdate {
            message: Arc::clone(&partial),
            assistant_message_event: AssistantMessageEvent::TextStart {
                content_index: 0,
                partial,
            },
        });
        for delta in ["ahead", " 🐈", "", "!"] {
            if delta != "ahead" {
                if let Content::Text(text) = &mut message.content[0] {
                    text.text.push_str(delta);
                }
            }
            let partial = Arc::new(message.clone());
            events.push(AgentEvent::MessageUpdate {
                message: Arc::clone(&partial),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.into(),
                    partial,
                },
            });
        }
        events
    }
    #[tokio::test]
    async fn volatile_recovery_matches_durable_prefixes_without_json_records() {
        let events = events();
        for prefix in 0..=events.len() {
            let durable = session(false);
            let volatile = session(true);
            for session in [&durable, &volatile] {
                let recorder = FrameRecordingEmitter::new(
                    Arc::new(CollectorEmitter::default()),
                    session.clone(),
                    "main",
                    "run",
                );
                for event in &events[..prefix] {
                    recorder.emit(event.clone()).await;
                }
            }
            assert_eq!(
                salvage_run_frames(&volatile, "run").await.unwrap(),
                salvage_run_frames(&durable, "run").await.unwrap(),
                "prefix {prefix}"
            );
            assert!(volatile
                .find_records(&RecordQuery {
                    record_type: Some(ASSISTANT_FRAME_RECORD_TYPE),
                    ..Default::default()
                })
                .await
                .unwrap()
                .is_empty());
        }
    }
    #[tokio::test]
    async fn volatile_bindings_clear_and_storage_switch_preserve_commit_boundaries() {
        for volatile in [false, true] {
            let session = session(volatile);
            let recorder = FrameRecordingEmitter::new(
                Arc::new(CollectorEmitter::default()),
                session.clone(),
                "main",
                "run",
            );
            for event in events() {
                recorder.emit(event).await;
            }
            assert_eq!(replay_run_frames(&session, "run").await.unwrap().len(), 1);
            bind_stream_entry(&session, "main", "run", 0, "settled")
                .await
                .unwrap();
            assert_eq!(
                replay_run_frames(&session, "run").await.unwrap().len(),
                1,
                "a binding alone must not hide progress"
            );
            session
                .storage()
                .append_entry(
                    ProvisionedEntry {
                        id: "settled".into(),
                        kind: ProvisionedKind::Message {
                            message: rpi_ai::UserMessage::new("committed", 42).into(),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            assert!(replay_run_frames(&session, "run").await.unwrap().is_empty());
            recorder.clear_stream(0).await.unwrap();
            recorder.clear().await.unwrap();
            assert!(replay_run_frames(&session, "run").await.unwrap().is_empty());
            if volatile {
                assert!(session
                    .volatile_progress()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .runs
                    .is_empty());
                let other = self::session(false);
                session.set_storage(other.storage());
                assert!(session.volatile_progress().is_none());
            }
        }
    }

    #[tokio::test]
    async fn harness_switch_adopts_the_target_recovery_cache_and_existing_views() {
        use crate::agent_harness::AgentHarness;
        use crate::types::AgentHarnessOptions;
        for target_volatile in [false, true] {
            let source = session(true);
            let harness = AgentHarness::create(AgentHarnessOptions {
                session: source,
                ..Default::default()
            })
            .await
            .unwrap();
            let existing_view = harness.session().clone();
            let old = FrameRecordingEmitter::new(
                Arc::new(CollectorEmitter::default()),
                existing_view.clone(),
                "main",
                "old-run",
            );
            for event in events() {
                old.emit(event).await;
            }
            let target = session(target_volatile);
            let recorder = FrameRecordingEmitter::new(
                Arc::new(CollectorEmitter::default()),
                target.clone(),
                "main",
                "target-run",
            );
            for event in events() {
                recorder.emit(event).await;
            }
            bind_stream_entry(&target, "main", "target-run", 0, "target-entry")
                .await
                .unwrap();
            let expected = salvage_run_frames(&target, "target-run").await.unwrap();
            assert_eq!(expected.len(), 1);
            harness.set_session(target.clone()).await.unwrap();
            assert_eq!(
                salvage_run_frames(harness.session(), "target-run")
                    .await
                    .unwrap(),
                expected
            );
            assert_eq!(
                salvage_run_frames(&existing_view, "target-run")
                    .await
                    .unwrap(),
                expected
            );
            assert!(salvage_run_frames(harness.session(), "old-run")
                .await
                .unwrap()
                .is_empty());
            target
                .storage()
                .append_entry(
                    ProvisionedEntry {
                        id: "target-entry".into(),
                        kind: ProvisionedKind::Message {
                            message: rpi_ai::UserMessage::new("committed", 42).into(),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            assert!(
                salvage_run_frames(harness.session(), "target-run")
                    .await
                    .unwrap()
                    .is_empty(),
                "the target's entry binding must survive the switch"
            );
        }
    }
}
