//! Borrowed discrete-event simulation. Each step prepares a runnable batch,
//! executes it with bounded parallelism, and commits outputs in scheduling order.
//! Modeled durations occupy virtual pool slots; output timestamps are invocation
//! times. Ready work runs before advancing to the next future event.
//!
//! Every callback must explicitly configure its modeled execution duration, using
//! `CallbackSchedule::with_execution_duration` or a duration callback. Construction
//! rejects missing durations; explicitly selected zero durations are supported.
//!
//! Real worker batches are scoped to individual steps. A single real worker runs
//! inline, independently of how many virtual threads the simulation models.
pub mod executor;
pub mod state;
pub use executor::SimulationExecutor;
pub use state::{SimulationState, StepError, StepResult};
pub use task::time::FrameworkTime;

/// Simulated pool sizes are independent of the real workers executing a batch.
#[derive(Clone, Debug)]
pub struct SimulationConfig {
    pub start_time: FrameworkTime,
    pub virtual_pool_threads: Vec<usize>,
    pub node_executor_thread_count: usize,
    /// Poll real IPC event listeners at step boundaries. Disable this when
    /// scheduled injections are the authoritative source of simulation events.
    pub poll_external_events: bool,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            start_time: FrameworkTime::from_nanoseconds(0),
            virtual_pool_threads: vec![1],
            node_executor_thread_count: 1,
            poll_external_events: true,
        }
    }
}
