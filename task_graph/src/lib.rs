//! Named task registration, graph construction, and capture/replay planning.
mod application;
pub mod graph;
mod named;

pub use named::{bundle, capture, exact, port_capture, replay, topology};

pub use application::{ApplicationBuilder, ApplicationPlan, ApplicationStorage};
pub use graph::{
    BuiltGraph, CallbackSchedule, FactoryError, GraphBuildError, GraphBuilder, GraphMetadata,
    ScheduledCallback, TimingError,
};
pub use named::{
    BuildError, ChannelOverrides, NamedBindings, NamedPlan, NamedStorage, PortDirection,
    RegisteredTask, Task, TaskFactory, TaskRegistration,
};
pub use topology::Topology;
