//! Opt-in integration-test observation of the *synchronous C emit* queue.
//! Markers identify requests, so unrelated concurrent providers cannot satisfy a
//! handshake. Only weak references are retained: no extra sender/receiver owner.
/// Read the invocation captured at the actual native callback boundary.
pub fn native_identity() -> Option<rpi_ai::identity::RequestIdentity> {
    crate::ActionBridge::native_invocation(0).and_then(|bridge| bridge.operation.clone())
}

use super::{queue, StreamChunk};
use std::sync::{Mutex, OnceLock, Weak};
type Queues = std::collections::HashMap<String, Weak<queue::Queue>>;
fn queues() -> &'static Mutex<Queues> {
    static QUEUES: OnceLock<Mutex<Queues>> = OnceLock::new();
    QUEUES.get_or_init(Mutex::default)
}
pub(super) fn observe(marker: &str, sender: &crate::bounded::Sender<StreamChunk>) {
    let mut queues = queues().lock().unwrap();
    queues.retain(|_, queue| queue.strong_count() != 0);
    queues.insert(marker.into(), sender.downgrade());
}
/// Locks the queue state: a nonzero count proves `Condvar::wait` has released
/// that same lock while the C emit's capacity predicate was true.
pub fn pending_emit(marker: &str) -> bool {
    queues()
        .lock()
        .unwrap()
        .get(marker)
        .and_then(Weak::upgrade)
        .is_some_and(|queue| queue.capacity_waiters() > 0)
}

/// Observe storage without resolving authority or initiating reclamation.
pub fn continuation_storage(id: &str) -> Option<(bool, usize, usize, usize)> {
    super::wrappers::observe_storage(id)
}
