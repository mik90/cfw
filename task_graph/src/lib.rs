//! Task registration, graph construction, and capture/replay planning.
mod application;
pub mod bundle;
pub mod capture;
pub mod endpoints;
pub mod execution_log;
pub mod graph;
mod plan;
pub mod pool;
pub mod replay;
pub mod topology;

pub use application::ApplicationBuilder;
pub use graph::{
    BuiltGraph, CallbackSchedule, FactoryError, GraphBuildError, GraphBuilder, GraphMetadata,
    ScheduledCallback, TimingError,
};
pub use plan::{
    BuildError, ChannelOverrides, EndpointDirection, GraphBindings, GraphPlan, GraphStorage, Task,
    TaskBinding, TaskRegistration,
};
pub use task::ChannelName;
pub use topology::Topology;
