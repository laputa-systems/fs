//! Bounded parallel worker coordination.
//!
//! The queue in this module deliberately has a small API.  It is a blocking,
//! multi-producer/multi-consumer queue intended for filesystem work, where
//! bounded outstanding work is more important than squeezing the last bit of
//! throughput out of the scheduler.  Cancellation is checked while a sender
//! or receiver is waiting, so a fatal operation can stop a blocked walker or
//! worker without requiring an async runtime.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// An error shared by the cancellation state and all workers.
pub type SharedError = Arc<dyn Error + Send + Sync + 'static>;

const WAIT_POLL: Duration = Duration::from_millis(25);

struct CancellationInner {
    cancelled: AtomicBool,
    reason: Mutex<Option<SharedError>>,
    wake: Condvar,
    #[cfg(test)]
    wake_lock: Mutex<()>,
}

/// Shared first-error cancellation state.
///
/// Cancellation is idempotent.  If several workers fail at once, the first
/// published reason wins; this gives callers one stable diagnostic while all
/// workers still observe the same stop request.
#[derive(Clone)]
pub struct Cancellation {
    inner: Arc<CancellationInner>,
}

impl fmt::Debug for Cancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Cancellation {
    /// Create a new, uncancelled state.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancellationInner {
                cancelled: AtomicBool::new(false),
                reason: Mutex::new(None),
                wake: Condvar::new(),
                #[cfg(test)]
                wake_lock: Mutex::new(()),
            }),
        }
    }

    /// Request cancellation without attaching an error.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.wake.notify_all();
    }

    /// Request cancellation and publish the first fatal error.
    pub fn cancel_with_error<E>(&self, error: E)
    where
        E: Error + Send + Sync + 'static,
    {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            *self
                .inner
                .reason
                .lock()
                .expect("cancellation reason poisoned") = Some(Arc::new(error));
        }
        self.inner.wake.notify_all();
    }

    /// Whether cancellation has been requested.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Return the first published error, if any.
    pub fn reason(&self) -> Option<SharedError> {
        self.inner
            .reason
            .lock()
            .expect("cancellation reason poisoned")
            .clone()
    }

    /// Block until cancellation is requested.
    #[cfg(test)]
    pub fn wait(&self) {
        let mut guard = self.inner.wake_lock.lock().expect("cancellation poisoned");
        while !self.is_cancelled() {
            guard = self.inner.wake.wait(guard).expect("cancellation poisoned");
        }
    }
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

struct QueueState<T> {
    items: VecDeque<T>,
    closed: bool,
    senders: usize,
    receivers: usize,
}

struct SharedQueue<T> {
    capacity: usize,
    state: Mutex<QueueState<T>>,
    not_empty: Condvar,
    not_full: Condvar,
    cancellation: Cancellation,
}

/// Error returned when a queued item cannot be sent.  The item is returned so
/// callers can clean up any operation-specific resources it owns.
#[derive(Debug, PartialEq, Eq)]
pub enum SendError<T> {
    /// The queue has been explicitly closed or has no receiver.
    Closed(T),
    /// Shared cancellation was requested before the item could be queued.
    Cancelled(T),
}

/// Error returned by [`WorkReceiver::recv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// Shared cancellation was requested.  Queued work is intentionally
    /// abandoned so workers do not continue mutating unrelated paths.
    Cancelled,
}

/// The sending half of a bounded work queue.
pub struct WorkSender<T> {
    shared: Arc<SharedQueue<T>>,
}

impl<T> Clone for WorkSender<T> {
    fn clone(&self) -> Self {
        let mut state = self.shared.state.lock().expect("work queue poisoned");
        state.senders += 1;
        drop(state);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> WorkSender<T> {
    /// Queue an item, waiting while the bounded queue is full.
    pub fn send(&self, item: T) -> Result<(), SendError<T>> {
        let mut item = Some(item);
        let mut state = self.shared.state.lock().expect("work queue poisoned");
        loop {
            if self.shared.cancellation.is_cancelled() {
                return Err(SendError::Cancelled(item.take().expect("item present")));
            }
            if state.closed || state.receivers == 0 {
                return Err(SendError::Closed(item.take().expect("item present")));
            }
            if state.items.len() < self.shared.capacity {
                state.items.push_back(item.take().expect("item present"));
                self.shared.not_empty.notify_one();
                return Ok(());
            }
            let (next, _) = self
                .shared
                .not_full
                .wait_timeout(state, WAIT_POLL)
                .expect("work queue poisoned");
            state = next;
        }
    }

    /// Stop accepting new items while allowing receivers to drain queued work.
    pub fn close(&self) {
        close_queue(&self.shared);
    }
}

impl<T> Drop for WorkSender<T> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().expect("work queue poisoned");
        state.senders = state.senders.saturating_sub(1);
        if state.senders == 0 {
            self.shared.not_empty.notify_all();
        }
    }
}

/// The receiving half of a bounded work queue.
pub struct WorkReceiver<T> {
    shared: Arc<SharedQueue<T>>,
}

impl<T> Clone for WorkReceiver<T> {
    fn clone(&self) -> Self {
        let mut state = self.shared.state.lock().expect("work queue poisoned");
        state.receivers += 1;
        drop(state);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> WorkReceiver<T> {
    /// Receive the next item.  `Ok(None)` means the queue is closed and
    /// drained; cancellation is returned separately so workers can distinguish
    /// a normal end of discovery from an aborted operation.
    pub fn recv(&self) -> Result<Option<T>, RecvError> {
        let mut state = self.shared.state.lock().expect("work queue poisoned");
        loop {
            if self.shared.cancellation.is_cancelled() {
                return Err(RecvError::Cancelled);
            }
            if let Some(item) = state.items.pop_front() {
                self.shared.not_full.notify_one();
                return Ok(Some(item));
            }
            if state.closed || state.senders == 0 {
                return Ok(None);
            }
            let (next, _) = self
                .shared
                .not_empty
                .wait_timeout(state, WAIT_POLL)
                .expect("work queue poisoned");
            state = next;
        }
    }

    /// Number of items currently waiting in the queue.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.shared
            .state
            .lock()
            .expect("work queue poisoned")
            .items
            .len()
    }
}

impl<T> Drop for WorkReceiver<T> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().expect("work queue poisoned");
        state.receivers = state.receivers.saturating_sub(1);
        if state.receivers == 0 {
            self.shared.not_full.notify_all();
        }
    }
}

fn close_queue<T>(shared: &Arc<SharedQueue<T>>) {
    let mut state = shared.state.lock().expect("work queue poisoned");
    state.closed = true;
    shared.not_empty.notify_all();
    shared.not_full.notify_all();
}

/// A bounded queue together with its shared cancellation state.
pub struct WorkQueue<T> {
    shared: Arc<SharedQueue<T>>,
}

impl<T> WorkQueue<T> {
    /// Construct a queue.  A zero capacity cannot make progress and is
    /// rejected explicitly instead of becoming an accidental rendezvous
    /// channel.
    #[cfg(test)]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "work queue capacity must be non-zero");
        Self::with_cancellation(capacity, Cancellation::new())
    }

    /// Construct a queue using an existing cancellation state.
    pub fn with_cancellation(capacity: usize, cancellation: Cancellation) -> Self {
        assert!(capacity > 0, "work queue capacity must be non-zero");
        Self {
            shared: Arc::new(SharedQueue {
                capacity,
                state: Mutex::new(QueueState {
                    items: VecDeque::with_capacity(capacity),
                    closed: false,
                    senders: 0,
                    receivers: 0,
                }),
                not_empty: Condvar::new(),
                not_full: Condvar::new(),
                cancellation,
            }),
        }
    }

    /// Obtain a sending handle.  The queue itself does not retain an implicit
    /// sender, which makes dropping the final handle a natural end-of-input
    /// signal for receivers.
    pub fn sender(&self) -> WorkSender<T> {
        let mut state = self.shared.state.lock().expect("work queue poisoned");
        state.senders += 1;
        drop(state);
        WorkSender {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Obtain a receiving handle.  Receivers may be cloned for worker threads.
    pub fn receiver(&self) -> WorkReceiver<T> {
        let mut state = self.shared.state.lock().expect("work queue poisoned");
        state.receivers += 1;
        drop(state);
        WorkReceiver {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The cancellation state shared by this queue.
    #[cfg(test)]
    pub fn cancellation(&self) -> Cancellation {
        self.shared.cancellation.clone()
    }
}

/// Error returned when joining a worker group.
#[derive(Debug)]
pub enum WorkerJoinError {
    /// A worker thread panicked.  The panic payload is intentionally not
    /// propagated across the filesystem error boundary.
    Panicked,
    /// A worker published a fatal task error.
    Task(SharedError),
    /// The group was cancelled without an attached task error.
    Cancelled,
}

impl fmt::Display for WorkerJoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panicked => f.write_str("worker thread panicked"),
            Self::Task(error) => write!(f, "worker task failed: {error}"),
            Self::Cancelled => f.write_str("workers cancelled"),
        }
    }
}

impl Error for WorkerJoinError {}

/// A group of ordinary OS threads consuming one queue.
pub struct WorkerGroup {
    handles: Vec<JoinHandle<()>>,
    cancellation: Cancellation,
}

impl WorkerGroup {
    /// Spawn `jobs` workers.  Each task error cancels the queue and prevents
    /// further mutations from being scheduled by other workers.
    pub fn spawn<T, F, E>(
        receiver: WorkReceiver<T>,
        jobs: usize,
        cancellation: Cancellation,
        task: F,
    ) -> Self
    where
        T: Send + 'static,
        F: Fn(T) -> Result<(), E> + Send + Sync + 'static,
        E: Error + Send + Sync + 'static,
    {
        assert!(jobs > 0, "worker count must be non-zero");
        let task = Arc::new(task);
        let mut handles = Vec::with_capacity(jobs);
        for _ in 0..jobs {
            let worker_receiver = receiver.clone();
            let worker_cancel = cancellation.clone();
            let worker_task = Arc::clone(&task);
            handles.push(thread::spawn(move || {
                loop {
                    let item = match worker_receiver.recv() {
                        Ok(Some(item)) => item,
                        Ok(None) | Err(RecvError::Cancelled) => break,
                    };
                    if let Err(error) = worker_task(item) {
                        worker_cancel.cancel_with_error(error);
                        break;
                    }
                }
            }));
        }
        drop(receiver);
        Self {
            handles,
            cancellation,
        }
    }

    /// Join every worker and report the first failure after all workers stop.
    pub fn join(self) -> Result<(), WorkerJoinError> {
        let mut panicked = false;
        for handle in self.handles {
            if handle.join().is_err() {
                panicked = true;
            }
        }
        if panicked {
            return Err(WorkerJoinError::Panicked);
        }
        if let Some(error) = self.cancellation.reason() {
            return Err(WorkerJoinError::Task(error));
        }
        if self.cancellation.is_cancelled() {
            return Err(WorkerJoinError::Cancelled);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn queue_never_exceeds_capacity_and_applies_backpressure() {
        let queue = WorkQueue::new(1);
        let sender = queue.sender();
        let receiver = queue.receiver();
        sender.send(1).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let blocked = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            sender.send(2)
        });
        ready_rx.recv().unwrap();
        thread::sleep(Duration::from_millis(20));
        assert_eq!(receiver.len(), 1);
        assert_eq!(receiver.recv().unwrap(), Some(1));
        assert!(blocked.join().unwrap().is_ok());
        assert_eq!(receiver.recv().unwrap(), Some(2));
    }

    #[test]
    fn cancellation_releases_a_blocked_sender_with_its_item() {
        let queue = WorkQueue::new(1);
        let sender = queue.sender();
        let receiver = queue.receiver();
        sender.send(1).unwrap();
        let cancel = queue.cancellation();
        let blocked = thread::spawn(move || sender.send(2));
        thread::sleep(Duration::from_millis(20));
        cancel.cancel();
        assert_eq!(blocked.join().unwrap(), Err(SendError::Cancelled(2)));
        assert_eq!(receiver.recv(), Err(RecvError::Cancelled));
    }

    #[test]
    fn dropping_last_sender_closes_after_draining() {
        let queue = WorkQueue::new(2);
        let sender = queue.sender();
        let receiver = queue.receiver();
        sender.send(7).unwrap();
        drop(sender);
        assert_eq!(receiver.recv().unwrap(), Some(7));
        assert_eq!(receiver.recv().unwrap(), None);
    }

    #[test]
    fn worker_group_cancels_on_first_task_error() {
        let queue = WorkQueue::new(8);
        let sender = queue.sender();
        let receiver = queue.receiver();
        let cancellation = queue.cancellation();
        for item in 0..8 {
            sender.send(item).unwrap();
        }
        drop(sender);
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_task = Arc::clone(&seen);
        let group = WorkerGroup::spawn(receiver, 3, cancellation.clone(), move |item| {
            seen_task.fetch_add(1, Ordering::Relaxed);
            if item == 0 {
                Err(std::io::Error::other("stop"))
            } else {
                Ok(())
            }
        });
        let joined = group.join();
        assert!(matches!(joined, Err(WorkerJoinError::Task(_))));
        assert!(cancellation.is_cancelled());
        assert!(seen.load(Ordering::Relaxed) <= 8);
    }

    #[test]
    fn cancellation_wait_is_prompt() {
        let cancellation = Cancellation::new();
        let clone = cancellation.clone();
        let start = Instant::now();
        let thread = thread::spawn(move || {
            clone.wait();
        });
        thread::sleep(Duration::from_millis(10));
        cancellation.cancel();
        thread.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
