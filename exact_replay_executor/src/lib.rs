//! Deterministic recorded-execution replay over externally owned graph storage.
//! Plan isolated hydration publishers before allocation, install per-endpoint captures
//! before binding callbacks, and step recorded input snapshots in timestamp order.
pub mod plan;
pub mod replay_plan;
pub use replay_plan::{ExactReplayPlan, ExplicitReplayEndpoint, GraphSourceCachePlan};
mod replay;
mod replay_log;
pub mod report;
pub use plan::{
    BundleReplayInputPlan, ReplayBindings, ReplayInputPlan, SourceCache, SourceCachePlan,
};
pub use replay::{
    DivergencePolicy, ExactReplayConfig, ExactReplayExecutor, ReplayError, ReplayStep, StopSignal,
};
pub use replay_log::ReplayLog;
pub use report::{ChannelStats, ReplayDetail, ReplayDetailKind, ReplayReport};
