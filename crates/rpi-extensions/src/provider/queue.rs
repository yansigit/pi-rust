//! Provider cancellation ownership around the shared bounded foreign-call queue.
use super::{ProviderControlState, StreamChunk};
use std::sync::Arc;

pub(super) type Queue = crate::bounded::Queue<StreamChunk>;
pub(super) type Sender = crate::bounded::Sender<StreamChunk>;
pub(super) struct Receiver {
    inner: crate::bounded::Receiver<StreamChunk>,
    control: Arc<ProviderControlState>,
}
#[cfg(all(test, feature = "provider-performance"))]
pub(super) use crate::bounded::peak_occupancy;

pub(super) fn channel(
    capacity: usize,
    byte_limit: usize,
    control: Arc<ProviderControlState>,
) -> (Sender, Receiver) {
    let (sender, inner) = crate::bounded::channel(capacity, byte_limit);
    *control.queue.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender.downgrade());
    (sender, Receiver { inner, control })
}
impl Receiver {
    pub(super) async fn recv(&mut self) -> Option<StreamChunk> {
        self.inner.recv().await
    }
    pub(super) fn recv_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<StreamChunk>, ()> {
        self.inner.recv_timeout(timeout)
    }
    pub(super) fn try_recv(&mut self) -> Result<StreamChunk, ()> {
        self.inner.try_recv()
    }
    pub(super) fn close(&mut self) {
        self.inner.close();
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.inner.close();
        self.control.cancel();
    }
}
