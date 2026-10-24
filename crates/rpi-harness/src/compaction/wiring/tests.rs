//! Wiring regressions: host paths/assistant messages must reach the pure
//! decisions unchanged, and the projected preparation must agree with the host
//! preparation on sessions without context edits.

use super::*;
use crate::session::types::{BranchSummaryEntry, CompactionEntry, EntryBase, MessageEntry};
use rpi_agent::message::AgentMessage;
use rpi_ai::model::Model;
use rpi_ai::providers::faux::faux_assistant_message;
use rpi_ai::types::{StopReason, Usage, UserContent, UserMessage};

fn model(context_window: u64) -> Model {
    let mut model = Model::new(
        "fixture",
        "fixture",
        rpi_ai::types::Api::Faux,
        "fixture",
        "",
    );
    model.context_window = context_window;
    model.max_tokens = 4096;
    model
}

fn settings(keep_recent_tokens: i64) -> CompactionSettings {
    CompactionSettings {
        enabled: true,
        reserve_tokens: 1000,
        keep_recent_tokens,
    }
}

fn user(id: &str, seq: u64, parent: Option<&str>, text: &str) -> Entry {
    Entry::Message(MessageEntry {
        base: EntryBase {
            entry_type: "message".into(),
            id: id.into(),
            seq,
            parent_id: parent.map(ToOwned::to_owned),
            timestamp: seq as i64,
        },
        message: AgentMessage::User(UserMessage::new(UserContent::Text(text.into()), seq as i64)),
        terminate: None,
    })
}

fn assistant(id: &str, seq: u64, parent: Option<&str>, text: &str, stop: StopReason) -> Entry {
    let mut message = faux_assistant_message(text, stop);
    message.timestamp = seq as i64;
    Entry::Message(MessageEntry {
        base: EntryBase {
            entry_type: "message".into(),
            id: id.into(),
            seq,
            parent_id: parent.map(ToOwned::to_owned),
            timestamp: seq as i64,
        },
        message: AgentMessage::Assistant(Box::new(message)),
        terminate: None,
    })
}

fn compaction(id: &str, seq: u64, parent: &str, tail: Vec<AgentMessage>) -> Entry {
    Entry::Compaction(CompactionEntry {
        base: EntryBase {
            entry_type: "compaction".into(),
            id: id.into(),
            seq,
            parent_id: Some(parent.into()),
            timestamp: seq as i64,
        },
        summary: "earlier summary".into(),
        retained_tail: tail,
        tokens_before: 500,
        details: None,
        usage: None,
    })
}

#[test]
fn retained_tail_is_spliced_so_the_projection_matches_the_host_replay() {
    let tail = vec![AgentMessage::User(UserMessage::new(
        UserContent::Text("kept question".into()),
        2,
    ))];
    let path = vec![
        user("u1", 1, None, "old question"),
        compaction("c1", 2, "u1", tail),
        assistant("a2", 3, Some("c1"), "kept answer", StopReason::Stop),
    ];
    let values = pi_path_values(&path);
    // The compaction records a first kept id pointing at the spliced entry, and
    // the following path entry now descends from the spliced tail.
    assert_eq!(values[1]["firstKeptEntryId"], "c1:retained:0");
    assert_eq!(values[2]["id"], "c1:retained:0");
    assert_eq!(values[3]["parentId"], "c1:retained:0");

    let projection = build_session_projection(&values, &LeafSelector::Default);
    let host = build_session_context(&path).messages;
    assert_eq!(projection.messages.len(), host.len());
    // Order and roles agree: compaction summary, retained tail, following turn.
    let roles = |messages: &[serde_json::Value]| -> Vec<String> {
        messages
            .iter()
            .map(|message| {
                message
                    .get("role")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    };
    assert_eq!(
        roles(&projection.messages),
        vec!["compactionSummary", "user", "assistant"]
    );
}

#[test]
fn projected_preparation_matches_the_host_cut_without_compaction() {
    // Four turns; keep_recent small enough to split after the first two.
    let mut path = vec![user("u1", 1, None, "one")];
    let mut parent = "u1".to_string();
    for (index, (question, answer)) in [
        ("two", "two answer"),
        ("three", "three answer"),
        ("four", "four answer"),
    ]
    .iter()
    .enumerate()
    {
        let user_id = format!("u{}", index + 2);
        let assistant_id = format!("a{}", index + 2);
        path.push(assistant(
            &assistant_id,
            (index as u64) * 2 + 2,
            Some(&parent),
            answer,
            StopReason::Stop,
        ));
        path.push(user(
            &user_id,
            (index as u64) * 2 + 3,
            Some(&assistant_id),
            question,
        ));
        parent = user_id;
    }
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 1000,
        keep_recent_tokens: 1,
    };
    let projected = projected_preparation(&path, settings).expect("preparation");
    let host = crate::compaction::prepare_compaction(&path, settings)
        .unwrap()
        .expect("host preparation");
    assert_eq!(
        projected.messages_to_summarize, host.messages_to_summarize,
        "the projected port must summarize the same host messages"
    );
    assert_eq!(projected.is_split_turn, host.is_split_turn);
    assert_eq!(projected.retained_tail, host.retained_tail);
    assert_eq!(projected.file_ops, host.file_ops);
}

#[test]
fn projected_preparation_skips_an_already_compacted_tail() {
    let path = vec![
        user("u1", 1, None, "gone"),
        compaction("c1", 2, "u1", Vec::new()),
    ];
    assert!(projected_preparation(&path, settings(1)).is_none());
}

#[test]
fn threshold_decision_matches_the_pure_check() {
    let path = vec![
        user("u1", 1, None, &"large ".repeat(1200)),
        assistant("a1", 2, Some("u1"), "answer", StopReason::Stop),
    ];
    let messages: Vec<AgentMessage> = path
        .iter()
        .filter_map(|entry| crate::compaction::get_message_from_entry(entry))
        .collect();
    let model = model(2_000);
    let decision = auto_compaction_decision(&AutoCompactionCheck {
        path: &path,
        messages: &messages,
        model: &model,
        settings: settings(1),
        assistant_entry_id: Some("a1"),
        skip_aborted_check: true,
        overflow_recovery_attempted: false,
    });
    assert_eq!(
        decision,
        AutoCompactionDecision::Run {
            reason: CompactionReason::Threshold,
            will_retry: false
        }
    );
}

#[test]
fn overflow_error_runs_once_then_reports_exhausted() {
    let mut message = faux_assistant_message("", StopReason::Error);
    message.error_message = Some("prompt is too long: 300000 tokens > 200000 maximum".into());
    message.provider = "fixture".into();
    message.model = "fixture".into();
    let messages = vec![AgentMessage::Assistant(Box::new(message))];
    let model = model(200_000);
    let first = auto_compaction_decision(&AutoCompactionCheck {
        path: &[],
        messages: &messages,
        model: &model,
        settings: settings(1),
        assistant_entry_id: None,
        skip_aborted_check: true,
        overflow_recovery_attempted: false,
    });
    assert_eq!(
        first,
        AutoCompactionDecision::Run {
            reason: CompactionReason::Overflow,
            will_retry: true
        }
    );
    let second = auto_compaction_decision(&AutoCompactionCheck {
        path: &[],
        messages: &messages,
        model: &model,
        settings: settings(1),
        assistant_entry_id: None,
        skip_aborted_check: true,
        overflow_recovery_attempted: true,
    });
    assert_eq!(
        second,
        AutoCompactionDecision::Exhausted {
            reason: CompactionReason::Overflow
        }
    );
}

#[test]
fn aborted_messages_are_skipped_only_when_the_check_says_so() {
    let message = faux_assistant_message("", StopReason::Aborted);
    let messages = vec![AgentMessage::Assistant(Box::new(message))];
    let model = model(1_000);
    let check = |skip_aborted_check| {
        auto_compaction_decision(&AutoCompactionCheck {
            path: &[],
            messages: &messages,
            model: &model,
            settings: settings(1),
            assistant_entry_id: None,
            skip_aborted_check,
            overflow_recovery_attempted: false,
        })
    };
    assert_eq!(check(true), AutoCompactionDecision::None);
    assert_eq!(check(false), AutoCompactionDecision::None);
}

#[test]
fn a_success_over_the_window_compacts_without_retry() {
    let mut message = faux_assistant_message("done", StopReason::Stop);
    message.provider = "fixture".into();
    message.model = "fixture".into();
    message.usage = Usage {
        input: 250_000,
        ..Usage::zero()
    };
    let path = vec![
        user("u1", 1, None, "question"),
        Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".into(),
                id: "a1".into(),
                seq: 2,
                parent_id: Some("u1".into()),
                timestamp: 2,
            },
            message: AgentMessage::Assistant(Box::new(message)),
            terminate: None,
        }),
    ];
    let messages: Vec<AgentMessage> = path
        .iter()
        .filter_map(|entry| crate::compaction::get_message_from_entry(entry))
        .collect();
    let model = model(200_000);
    let decision = auto_compaction_decision(&AutoCompactionCheck {
        path: &path,
        messages: &messages,
        model: &model,
        settings: settings(1),
        assistant_entry_id: Some("a1"),
        skip_aborted_check: true,
        overflow_recovery_attempted: false,
    });
    assert_eq!(
        decision,
        AutoCompactionDecision::Run {
            reason: CompactionReason::Overflow,
            will_retry: false
        }
    );
}

#[test]
fn branch_summary_entries_reach_the_host_preparation() {
    let path = vec![
        Entry::BranchSummary(BranchSummaryEntry {
            base: EntryBase {
                entry_type: "branch_summary".into(),
                id: "b1".into(),
                seq: 1,
                parent_id: None,
                timestamp: 1,
            },
            from_id: "root".into(),
            summary: "branch".into(),
            details: None,
            usage: None,
            from_hook: None,
        }),
        user("u2", 2, Some("b1"), "new"),
        assistant("a2", 3, Some("u2"), "answer", StopReason::Stop),
    ];
    let preparation = projected_preparation(&path, settings(1)).expect("preparation");
    assert!(matches!(
        preparation.messages_to_summarize.first(),
        Some(AgentMessage::Custom(custom)) if custom.role == crate::messages::BRANCH_SUMMARY_ROLE
    ));
}
