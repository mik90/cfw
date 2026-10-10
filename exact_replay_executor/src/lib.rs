//! Deterministic recorded-execution replay over externally owned graph storage.
//! Plan isolated hydration publishers before allocation, install per-port captures
//! before binding callbacks, and step recorded input snapshots in timestamp order.
pub mod automatic;
pub mod plan;
pub use automatic::{AutomaticExactReplayPlan, AutomaticSourceCachePlan, ExplicitReplayPort};
mod replay;
mod replay_log;
pub mod report;
pub use plan::{ReplayBindings, ReplayInputPlan, SourceCache, SourceCachePlan};
pub use replay::{
    DivergencePolicy, ExactReplayConfig, ExactReplayExecutor, ReplayError, ReplayStep, StopSignal,
};
pub use replay_log::ReplayLog;
pub use report::{ChannelStats, ReplayReport};
