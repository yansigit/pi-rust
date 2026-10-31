//! Provider cancellation ownership around the shared bounded foreign-call queue.
use super::{ProviderControlState, StreamChunk};
use std::sync::Arc;

pub(super) type Queue = crate::bounded::Queue<StreamChunk>;
pub(super) type Sender = crate::bounded::Sender<StreamChunk>;
pub(super) struct Receiver {
    inner: crate::bounded::Receiver<StreamChunk>,
    control: Arc<ProviderControlState>,
    /// A chunk read while batching that could not join the batch. At most one
    /// chunk is held here, outside the queue's byte budget.
    lookahead: Option<StreamChunk>,
}
#[cfg(all(test, feature = "provider-performance"))]
pub(super) use crate::bounded::peak_occupancy;

// Only combine deltas that are already queued. This introduces no flush timer:
// a lone chunk is delivered immediately. Authoritative snapshots, metadata and
// unfamiliar fields stay separate, as do changes of content block or kind.
const MAX_DELTA_BATCH_BYTES: usize = 4096;

fn delta_identity(chunk: &StreamChunk) -> Option<(&str, u64, &str)> {
    let object = chunk.as_object()?;
    if object.len() != 3 {
        return None;
    }
    let kind = object.get("type")?.as_str()?;
    if !matches!(kind, "text_delta" | "thinking_delta") {
        return None;
    }
    Some((
        kind,
        object.get("contentIndex")?.as_u64()?,
        object.get("delta")?.as_str()?,
    ))
}

pub(super) fn channel(
    capacity: usize,
    byte_limit: usize,
    control: Arc<ProviderControlState>,
) -> (Sender, Receiver) {
    let (sender, inner) = crate::bounded::channel(capacity, byte_limit);
    *control.queue.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender.downgrade());
    (
        sender,
        Receiver {
            inner,
            control,
            lookahead: None,
        },
    )
}
impl Receiver {
    pub(super) async fn recv(&mut self) -> Option<StreamChunk> {
        let mut chunk = match self.lookahead.take() {
            Some(chunk) => chunk,
            None => self.inner.recv().await?,
        };
        if self.control.is_cancelled() {
            return None;
        }
        while let Some((kind, index, len)) =
            delta_identity(&chunk).map(|(kind, index, delta)| (kind.to_owned(), index, delta.len()))
        {
            let Ok(mut next) = self.inner.try_recv() else {
                break;
            };
            let joins = delta_identity(&next).is_some_and(|(next_kind, next_index, next_delta)| {
                kind == next_kind
                    && index == next_index
                    && len.saturating_add(next_delta.len()) <= MAX_DELTA_BATCH_BYTES
            });
            if !joins {
                self.lookahead = Some(next);
                break;
            }
            let serde_json::Value::String(next_delta) =
                next.as_object_mut().unwrap().remove("delta").unwrap()
            else {
                unreachable!("delta_identity checked the string")
            };
            if let serde_json::Value::String(delta) = &mut chunk["delta"] {
                delta.push_str(&next_delta);
            }
        }
        Some(chunk)
    }
    pub(super) fn recv_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<StreamChunk>, ()> {
        match self.lookahead.take() {
            Some(chunk) if !self.control.is_cancelled() => Ok(Some(chunk)),
            _ => self.inner.recv_timeout(timeout),
        }
    }
    pub(super) fn try_recv(&mut self) -> Result<StreamChunk, ()> {
        match self.lookahead.take() {
            Some(chunk) if !self.control.is_cancelled() => Ok(chunk),
            _ => self.inner.try_recv(),
        }
    }
    pub(super) fn close(&mut self) {
        self.lookahead = None;
        self.inner.close();
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.inner.close();
        self.control.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn delta(kind: &str, index: u64, text: &str) -> StreamChunk {
        serde_json::json!({"type":kind,"contentIndex":index,"delta":text})
    }

    #[tokio::test]
    async fn queued_unicode_deltas_share_a_snapshot_boundary_without_waiting_for_more() {
        for kind in ["text_delta", "thinking_delta"] {
            let (tx, mut rx) = channel(8, 4096, Arc::new(ProviderControlState::default()));
            for text in ["hello", "", " 🐈"] {
                tx.send(delta(kind, 2, text), 64).unwrap();
            }
            // Keep the sender open: a ready batch must not wait for another chunk.
            let chunk = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(chunk, delta(kind, 2, "hello 🐈"));
            assert!(rx.lookahead.is_none());
            assert!(rx.inner.try_recv().is_err());
            drop(tx);
            assert!(rx.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn batching_preserves_snapshot_metadata_block_and_unknown_field_boundaries() {
        let mut authoritative = delta("text_delta", 0, "replace");
        authoritative["partial"] = serde_json::json!({"content":[]});
        let mut unfamiliar = delta("text_delta", 0, "future");
        unfamiliar["signature"] = serde_json::json!("must survive");
        for boundary in [
            authoritative,
            unfamiliar,
            delta("text_delta", 1, "next block"),
            delta("thinking_delta", 0, "thinking"),
            delta("toolcall_delta", 0, "{}"),
            serde_json::json!({"type":"usage","usage":{"input":3}}),
            serde_json::json!({"type":"text_delta","contentIndex":0,"delta":null}),
            serde_json::json!({"type":"text_delta","delta":"missing index"}),
        ] {
            let (tx, mut rx) = channel(4, 4096, Arc::new(ProviderControlState::default()));
            let first = delta("text_delta", 0, "a");
            let last = delta("text_delta", 0, "b");
            for chunk in [&first, &boundary, &last] {
                tx.send(chunk.clone(), 100).unwrap();
            }
            drop(tx);
            assert_eq!(rx.recv().await, Some(first));
            assert_eq!(rx.recv().await, Some(boundary));
            assert_eq!(rx.recv().await, Some(last));
            assert!(rx.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn batches_respect_the_byte_cap_and_cancellation() {
        let control = Arc::new(ProviderControlState::default());
        let (tx, mut rx) = channel(8, 65536, Arc::clone(&control));
        let text = "x".repeat(MAX_DELTA_BATCH_BYTES / 2);
        for _ in 0..3 {
            tx.send(delta("text_delta", 0, &text), text.len()).unwrap();
        }
        assert_eq!(
            rx.recv().await,
            Some(delta("text_delta", 0, &text.repeat(2)))
        );
        assert_eq!(rx.lookahead, Some(delta("text_delta", 0, &text)));
        control.cancel();
        assert!(
            rx.recv().await.is_none(),
            "cancelled queued content is not forwarded"
        );
        rx.close();
        assert!(rx.lookahead.is_none());
    }

    #[tokio::test]
    async fn a_single_oversized_delta_is_delivered_unchanged() {
        let (tx, mut rx) = channel(2, 128, Arc::new(ProviderControlState::default()));
        let large = delta("text_delta", 0, &"x".repeat(MAX_DELTA_BATCH_BYTES + 1));
        tx.send(large.clone(), MAX_DELTA_BATCH_BYTES + 1).unwrap();
        assert_eq!(rx.recv().await, Some(large));
        let small = delta("text_delta", 0, "next");
        tx.send(small.clone(), 4).unwrap();
        assert_eq!(rx.recv().await, Some(small));
    }
}
