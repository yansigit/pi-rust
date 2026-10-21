//! Bounded synchronous-producer/async-consumer bridge. Payload is accounted in
//! incoming JSON bytes, not allocator overhead or total process memory.
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::Notify;

#[cfg(test)]
static PEAK_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static PEAK_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(all(test, feature = "provider-performance"))]
pub(crate) fn peak_occupancy() -> (usize, usize) {
    use std::sync::atomic::Ordering;
    (
        PEAK_COUNT.load(Ordering::Relaxed),
        PEAK_BYTES.load(Ordering::Relaxed),
    )
}

struct State<T> {
    chunks: VecDeque<(T, usize)>,
    bytes: usize,
    producer_open: bool,
    consumer_open: bool,
    cancelled: bool,
    peak_count: usize,
    peak_bytes: usize,
}
pub(crate) struct Queue<T> {
    state: Mutex<State<T>>,
    capacity: usize,
    byte_limit: usize,
    writable: Condvar,
    writable_async: Notify,
    readable: Notify,
    readable_blocking: Condvar,
}
pub(crate) struct Sender<T>(Arc<Queue<T>>);
pub(crate) struct Receiver<T> {
    queue: Arc<Queue<T>>,
}

pub(crate) fn channel<T>(capacity: usize, byte_limit: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0 && byte_limit > 0);
    let queue = Arc::new(Queue {
        state: Mutex::new(State {
            chunks: VecDeque::new(),
            bytes: 0,
            producer_open: true,
            consumer_open: true,
            cancelled: false,
            peak_count: 0,
            peak_bytes: 0,
        }),
        capacity,
        byte_limit,
        writable: Condvar::new(),
        writable_async: Notify::new(),
        readable: Notify::new(),
        readable_blocking: Condvar::new(),
    });
    (Sender(Arc::clone(&queue)), Receiver { queue })
}

impl<T> Queue<T> {
    pub(crate) fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cancelled = true;
        self.writable.notify_all();
        self.writable_async.notify_waiters();
        self.readable.notify_one();
        self.readable_blocking.notify_all();
    }
}
impl<T> Sender<T> {
    pub(crate) fn downgrade(&self) -> std::sync::Weak<Queue<T>> {
        Arc::downgrade(&self.0)
    }

    pub(crate) fn send(&self, chunk: T, bytes: usize) -> Result<(), ()> {
        let weight = bytes.max(1).min(self.0.byte_limit);
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.consumer_open
            && !state.cancelled
            && (state.chunks.len() == self.0.capacity || weight > self.0.byte_limit - state.bytes)
        {
            state = self
                .0
                .writable
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        if !state.consumer_open || state.cancelled {
            return Err(());
        }
        state.bytes += weight;
        state.chunks.push_back((chunk, weight));
        state.peak_count = state.peak_count.max(state.chunks.len());
        state.peak_bytes = state.peak_bytes.max(state.bytes);
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            PEAK_COUNT.fetch_max(state.chunks.len(), Ordering::Relaxed);
            PEAK_BYTES.fetch_max(state.bytes, Ordering::Relaxed);
        }
        self.0.readable.notify_one();
        self.0.readable_blocking.notify_all();
        Ok(())
    }

    pub(crate) async fn send_async(&self, chunk: T, bytes: usize) -> Result<(), ()> {
        let weight = bytes.max(1).min(self.0.byte_limit);
        loop {
            let ready = self.0.writable_async.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            {
                let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                if !state.consumer_open || state.cancelled {
                    return Err(());
                }
                if state.chunks.len() < self.0.capacity && weight <= self.0.byte_limit - state.bytes
                {
                    state.bytes += weight;
                    state.chunks.push_back((chunk, weight));
                    state.peak_count = state.peak_count.max(state.chunks.len());
                    state.peak_bytes = state.peak_bytes.max(state.bytes);
                    self.0.readable.notify_one();
                    self.0.readable_blocking.notify_all();
                    return Ok(());
                }
            }
            ready.await;
        }
    }
}
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .producer_open = false;
        self.0.readable.notify_one();
        self.0.readable_blocking.notify_all();
    }
}
impl<T> Receiver<T> {
    pub(crate) async fn recv(&mut self) -> Option<T> {
        loop {
            let ready = self.queue.readable.notified();
            {
                let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.cancelled || !state.consumer_open {
                    return None;
                }
                if let Some((chunk, bytes)) = state.chunks.pop_front() {
                    state.bytes -= bytes;
                    self.queue.writable.notify_all();
                    self.queue.writable_async.notify_waiters();
                    return Some(chunk);
                }
                if !state.producer_open {
                    return None;
                }
            }
            ready.await;
        }
    }

    /// Polling is only for a foreign cancellation control, never for capacity.
    pub(crate) fn recv_timeout(&mut self, timeout: std::time::Duration) -> Result<Option<T>, ()> {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if state.cancelled || !state.consumer_open {
                return Ok(None);
            }
            if let Some((chunk, bytes)) = state.chunks.pop_front() {
                state.bytes -= bytes;
                self.queue.writable.notify_all();
                self.queue.writable_async.notify_waiters();
                return Ok(Some(chunk));
            }
            if !state.producer_open {
                return Ok(None);
            }
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return Err(());
            };
            state = self
                .queue
                .readable_blocking
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    pub(crate) fn close(&mut self) {
        let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        state.consumer_open = false;
        state.chunks.clear();
        state.bytes = 0;
        self.queue.writable.notify_all();
        self.queue.writable_async.notify_waiters();
    }
    pub(crate) fn try_recv(&mut self) -> Result<T, ()> {
        let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        let (chunk, bytes) = state.chunks.pop_front().ok_or(())?;
        state.bytes -= bytes;
        self.queue.writable.notify_all();
        self.queue.writable_async.notify_waiters();
        Ok(chunk)
    }
}
impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.close();
        let state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        tracing::debug!(
            peak_events = state.peak_count,
            peak_payload_bytes = state.peak_bytes,
            "plugin bridge queue closed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn reversed_bridge_bounds_oversized_frames_and_cancellation_wakes_async_senders() {
        let (tx, mut rx) = channel(1, 4);
        let control = tx.downgrade().upgrade().unwrap();
        tx.send_async(serde_json::json!({"frame":0}), 100)
            .await
            .unwrap();
        let pending = tokio::spawn(async move {
            tx.send_async(serde_json::json!({"frame":1}), 2)
                .await
                .unwrap();
            tx.send_async(serde_json::json!({"frame":2}), 2).await
        });
        let frame = rx.recv_timeout(Duration::from_millis(1)).unwrap().unwrap();
        assert_eq!(frame["frame"], 0);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if rx.queue.state.lock().unwrap().chunks.len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        control.cancel();
        assert!(pending.await.unwrap().is_err());
        let state = rx.queue.state.lock().unwrap();
        assert_eq!(state.peak_count, 1);
        assert_eq!(state.peak_bytes, 4);
    }

    #[tokio::test]
    async fn capacity_one_dequeues_losslessly_and_wakes_blocking_producer() {
        let (tx, mut rx) = channel(1, 4);
        tx.send(serde_json::json!({"i":0}), 10).unwrap(); // oversized occupies the budget exclusively
        let thread = std::thread::spawn(move || {
            tx.send(serde_json::json!({"i":1}), 1).unwrap();
        });
        assert_eq!(rx.recv().await.unwrap()["i"], 0);
        assert_eq!(rx.recv().await.unwrap()["i"], 1);
        thread.join().unwrap();
        assert!(rx.recv().await.is_none());
        let state = rx.queue.state.lock().unwrap();
        assert_eq!(state.peak_count, 1);
        assert_eq!(state.peak_bytes, 4);
    }

    #[test]
    fn cancellation_and_receiver_drop_release_capacity_waits() {
        for cancel in [false, true] {
            let (tx, rx) = channel(1, 4);
            let control = tx.downgrade().upgrade().unwrap();
            tx.send(serde_json::json!({}), 4).unwrap();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                done_tx.send(tx.send(serde_json::json!({}), 1)).unwrap()
            });
            assert!(done_rx.recv_timeout(Duration::from_millis(10)).is_err());
            if cancel {
                control.cancel();
            } else {
                drop(rx);
            }
            assert!(done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err());
            thread.join().unwrap();
        }
    }
}
