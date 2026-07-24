use std::sync::Arc;

use crate::callback::CallbackNode;

pub struct ThreadPoolConfig {
    pub thread_count: usize,
    pub nodes: Vec<CallbackNode>,
}

impl ThreadPoolConfig {
    pub fn new(virtual_thread_count: usize, nodes: Vec<CallbackNode>) -> Self {
        ThreadPoolConfig {
            thread_count: virtual_thread_count,
            nodes,
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
