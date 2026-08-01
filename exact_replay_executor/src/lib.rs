//! Exact replay executor: replays a recorded execution log through the same
//! callback nodes, comparing actual outputs against expected outputs.
//!
//! # Architecture
//!
//! - [`config`] — [`ExactReplayConfig`] and meaningful defaults.
//! - [`descriptor`] — descriptor and descriptor-less execution validation.
//! - [`executor`] — [`ExactReplayExecutor`], [`StopSignal`], construction,
//!   lifecycle, and replay worker orchestration.
//! - [`log_reader`] — parses the log file and extracts the descriptor plus
//!   a time-ordered list of executions.
//! - [`scheduler`] — drives the replay loop, yielding one execution at a time.
//! - [`replay_task`] — manages persistent hydration publishers, capture
//!   subscribers, and the per-execution hydrate/run/compare logic.
//! - [`reproduce`] — storage for payloads reproduced from unlogged channels.
//! - [`report`] — the replay accuracy report.
//! - [`error`] — structured error types.
//!
//! # Re-exports
//!
//! - [`ExactReplayConfig`](config::ExactReplayConfig)
//! - [`ExactReplayExecutor`](executor::ExactReplayExecutor)
//! - [`DivergencePolicy`](replay_task::DivergencePolicy)
//! - [`ExactReplayExecutorError`](error::ExactReplayExecutorError)
//! - [`ReplayError`](error::ReplayError)
//! - [`ReplayReport`](report::ReplayReport)

pub mod config;
pub mod descriptor;
pub mod error;
pub(crate) mod executor;
pub(crate) mod log_reader;
pub(crate) mod replay_task;
pub(crate) mod report;
pub(crate) mod reproduce;
pub(crate) mod scheduler;

pub use config::ExactReplayConfig;
pub use error::{ExactReplayExecutorError, ReplayError};
pub use executor::ExactReplayExecutor;
pub use replay_task::DivergencePolicy;
pub use report::{ChannelStats, ReplayReport};
