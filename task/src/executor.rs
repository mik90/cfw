use std::sync::Arc;

use crate::callback_storage::CallbackStorage;
use crate::time::FrameworkTime;

#[derive(Debug)]
pub struct ThreadPoolConfig {
    pub thread_count: usize,
    pub nodes: CallbackStorage,
}

impl ThreadPoolConfig {
    /// Wrap either plain built nodes (`Vec<CallbackNode>`) or an existing
    /// [`CallbackStorage`] into a pool config.
    pub fn new(virtual_thread_count: usize, nodes: impl Into<CallbackStorage>) -> Self {
        ThreadPoolConfig {
            thread_count: virtual_thread_count,
            nodes: nodes.into(),
        }
    }
}

/// Allows a publisher to enqueue a callback node onto an executor without holding a lock
/// on the full executor state. Implementors must be Send + Sync.
pub trait CallbackNodeEnqueuer: Send + Sync {
    fn enqueue_node(&self, node_index: usize);
}

/// A non-blocking handle for signaling an executor to stop.
/// Takes `&self` so it can be held behind `Arc` and called from worker threads
/// without requiring a mutable lock on the executor itself.
pub trait ExecutorStopSignal: Send + Sync {
    fn request_stop(&self);
}

/// Source of monotonic time for an executor.
/// The default `WallClock` implementation uses `CLOCK_MONOTONIC`.
/// Replay executors substitute a log-driven time source that advances
/// at a configured multiplier over wall time so all callbacks see the
/// same replayed time stamp.
pub trait TimeSource: Send + Sync {
    fn now(&self) -> FrameworkTime;
}

/// Default wall-clock monotonic time source.
#[derive(Debug)]
pub struct WallClock;

impl TimeSource for WallClock {
    fn now(&self) -> FrameworkTime {
        FrameworkTime::from_wall_clock()
    }
}

pub trait Executor {
    type Error: std::error::Error;

    /// Start the executor. Callback nodes will begin running after this call.
    fn start(&mut self);

    /// Signal shutdown and block until all threads have joined.
    fn stop(&mut self) -> Result<(), Self::Error>;

    /// Return a shareable handle that can signal shutdown without blocking.
    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal>;

    /// Return whether the executor is still running.
    fn is_running(&self) -> bool;
}
