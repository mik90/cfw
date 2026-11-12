//! Bazel example for the C++ task API.
//!
//! Each task is generated as its own crate by `cfw_cxx_task`; this crate
//! re-exports them for the integration tests.

pub use fizz_buzz_calculator::FizzBuzzCalculator;
pub use norm_calculator::NormCalculator;
pub use optional_probe::OptionalProbe;
pub use span_probe::SpanProbe;

include!("tasks_tests.rs");
