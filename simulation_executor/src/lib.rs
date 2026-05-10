use std::collections::VecDeque;
mod callback_executor;
pub mod executor;
pub mod state;
use std::num::Saturating;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use task::callback::{ConnectedCallback, Run};
use task::context::Context;
use task::executor::{Executor, ExecutorError, ExecutorStopSignal, ThreadPoolConfig};
use task::time::FrameworkTime;

#[derive(Clone, Copy)]
struct TimeTriggeredTask {
    index: usize,
    requested_exec_time: FrameworkTime,
}

/// A virtual pool tracks how many concurrent "threads" it models,
/// without spawning real OS threads.
pub struct VirtualPool {
    /// Total count of threads in the pool
    virtual_thread_count: usize,

    /// How many threads are 'taken up' by a task until its busy_until time is reached
    num_threads_occupied: usize,
}

type PoolIndex = usize;
type TaskIndex = usize;

pub struct SimulationConfig {
    pub start_time: FrameworkTime,
    pub pools: Vec<ThreadPoolConfig>,
    /// Number of real OS threads used to execute callbacks in parallel within a step.
    /// Independent of any virtual thread pool sizes.
    pub callback_executor_thread_count: usize,
}
