use task::{LoanError, execute_callback, time::FrameworkTime};
use task_graph::{BuiltGraph, GraphMetadata, ScheduledCallback};

/// Test driver for explicit callback-order execution at a supplied timestamp.
/// Schedule policies are not interpreted; use the simulation executor for those.
pub struct SequentialExecutor<'storage> {
    callbacks: Vec<ScheduledCallback<'storage>>,
    metadata: GraphMetadata,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SequentialExecutionError {
    pub callback: String,
    pub source: LoanError,
}

impl<'storage> SequentialExecutor<'storage> {
    pub fn new(graph: BuiltGraph<'storage>) -> Self {
        let (callbacks, metadata) = graph.into_parts();
        Self {
            callbacks,
            metadata,
        }
    }

    pub fn metadata(&self) -> &GraphMetadata {
        &self.metadata
    }

    /// Stop at the first error; earlier publications are not rolled back.
    pub fn step(&mut self, timestamp: FrameworkTime) -> Result<(), SequentialExecutionError> {
        for node in &mut self.callbacks {
            execute_callback(node.callback.as_mut(), &self.metadata.context(timestamp)).map_err(
                |source| SequentialExecutionError {
                    callback: node.name.clone(),
                    source,
                },
            )?;
        }
        Ok(())
    }
}
