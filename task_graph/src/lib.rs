//! Task registration, graph construction, and capture/replay planning.
mod application;
pub mod bundle;
pub mod capture;
pub mod graph;
mod plan;
pub mod port_capture;
pub mod ports;
pub mod replay;
pub mod topology;

pub use application::ApplicationBuilder;
pub use graph::{
    BuiltGraph, CallbackSchedule, FactoryError, GraphBuildError, GraphBuilder, GraphMetadata,
    ScheduledCallback, TimingError,
};
pub use plan::{
    BuildError, ChannelOverrides, GraphBindings, GraphPlan, GraphStorage, PortDirection, Task,
    TaskBinding, TaskRegistration,
};
pub use topology::Topology;
