//! Graph capture planning for named graphs, before arena allocation.
//!
//! ```
//! use logging::{GraphCapturePlan, CaptureOptions};
//! use task_graph::GraphPlan;
//!
//! let mut plan = GraphPlan::default();
//! // Register tasks and their channel overrides here.
//! let captures = GraphCapturePlan::declare(
//!     &mut plan, &CaptureOptions::new(16).exclude("diagnostics"),
//! )?;
//! let storage = plan.allocate()?;
//! let bindings = storage.bind()?;
//! let captures = captures.bind(&bindings)?;
//! // Pass captures to LogSession; retain storage until execution and logging end.
//! # Ok::<(), task_graph::BuildError>(())
//! ```
use crate::Capture;
use std::collections::BTreeSet;
use task_graph::{BuildError, GraphBindings, GraphPlan, capture::NamedCapturePlan};

#[derive(Clone, Debug)]
pub struct CaptureOptions {
    pub capacity: usize,
    pub excluded_channels: BTreeSet<String>,
}
impl CaptureOptions {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            excluded_channels: BTreeSet::new(),
        }
    }
    /// Exclude a resolved channel name, including any task-instance override.
    pub fn exclude(mut self, channel: impl Into<String>) -> Self {
        self.excluded_channels.insert(channel.into());
        self
    }
}

pub struct GraphCapturePlan(NamedCapturePlan);
impl GraphCapturePlan {
    /// Register all tasks first. Each discovered data channel gets one capture.
    pub fn declare(plan: &mut GraphPlan, options: &CaptureOptions) -> Result<Self, BuildError> {
        Ok(Self(plan.declare_captures(
            options.capacity,
            &options.excluded_channels,
        )?))
    }
    pub fn channels(&self) -> &[String] {
        self.0.channels()
    }
    pub fn bind<'a>(self, bindings: &GraphBindings<'a>) -> Result<Vec<Capture<'a>>, BuildError> {
        Ok(self
            .0
            .bind(bindings)?
            .into_iter()
            .map(Capture::from_serialized)
            .collect())
    }
}
