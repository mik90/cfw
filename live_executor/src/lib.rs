pub mod error;
pub mod executor;
mod periodic;
mod pool_state;
#[cfg(feature = "iceoryx2")]
mod readiness;
pub mod stop_signal;

pub use error::{LiveExecutorError, LiveExecutorStartError, ThreadFailure};
pub use executor::LiveExecutor;
pub use stop_signal::StopSignal;

#[cfg(test)]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
