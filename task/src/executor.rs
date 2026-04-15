use std::sync::Arc;

use crate::callback::ConnectedCallback;

pub struct ThreadPoolConfig {
    pub thread_count: usize,
    pub tasks: Vec<ConnectedCallback>,
}

impl ThreadPoolConfig {
    pub fn new(virtual_thread_count: usize, tasks: Vec<ConnectedCallback>) -> Self {
        ThreadPoolConfig {
            thread_count: virtual_thread_count,
            tasks,
        }
    }
}

pub enum ExecutorError {
    PanickedThreads(Vec<usize>),
}

/// Allows a publisher to enqueue a task onto an executor without holding a lock
/// on the full executor state. Implementors must be Send + Sync.
pub trait TaskEnqueuer: Send + Sync {
    fn enqueue_task(&self, task_index: usize);
}

/// A non-blocking handle for signaling an executor to stop.
/// Takes `&self` so it can be held behind `Arc` and called from worker threads
/// without requiring a mutable lock on the executor itself.
pub trait ExecutorStopSignal: Send + Sync {
    fn request_stop(&self);
}

pub trait Executor {
    /// Start the executor. Tasks will begin running after this call.
    fn start(&mut self);

    /// Signal shutdown and block until all threads have joined.
    /// Returns Err with the indices of threads that panicked.
    fn stop(&mut self) -> Result<(), ExecutorError>;

    /// Return a shareable handle that can signal shutdown without blocking.
    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal>;

    /// Return whether the executor is still running.
    fn is_running(&self) -> bool;
}
