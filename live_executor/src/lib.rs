pub mod error;
pub mod executor;
pub mod periodic;
pub mod pool_state;
pub mod stop_signal;
pub mod worker_logger;

pub use error::LiveExecutorError;
pub use executor::LiveExecutor;
pub use stop_signal::StopSignal;

#[cfg(test)]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
