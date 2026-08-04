pub mod executor;
mod node_executor;
pub mod state;
#[cfg(feature = "log_simulation")]
pub mod log_simulation;
use task::callback::CallbackNode;
use task::context::Context;
use task::executor::ThreadPoolConfig;
use task::time::FrameworkTime;

#[derive(Clone, Copy)]
struct TimeTriggeredNode {
    index: usize,
    requested_exec_time: FrameworkTime,
}

/// A virtual pool tracks how many concurrent "threads" it models,
/// without spawning real OS threads.
pub struct VirtualPool {
    /// Total count of threads in the pool
    virtual_thread_count: usize,

    /// How many threads are 'taken up' by a callback node until its busy_until time is reached
    num_threads_occupied: usize,
}

type PoolIndex = usize;
type CallbackNodeIndex = usize;

pub struct SimulationConfig {
    pub start_time: FrameworkTime,
    pub pools: Vec<ThreadPoolConfig>,
    /// Number of real OS threads used to execute callback nodes in parallel within a step.
    /// Independent of any virtual thread pool sizes.
    pub node_executor_thread_count: usize,
}
