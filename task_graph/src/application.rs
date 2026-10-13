//! Declaration-driven construction with externally owned channel storage.
use crate::{BuildError, CallbackSchedule, GraphPlan, GraphStorage, Task, TaskRegistration};

/// Collect tasks whose channel annotations provide the default wiring.
#[derive(Default)]
pub struct ApplicationBuilder {
    tasks: Vec<TaskRegistration>,
}

impl ApplicationBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure per-instance wiring through the returned registration.
    pub fn add_task(
        &mut self,
        name: impl Into<String>,
        task: impl Task + 'static,
        schedule: CallbackSchedule,
    ) -> &mut TaskRegistration {
        self.tasks.push(TaskRegistration::new(name, task, schedule));
        self.tasks.last_mut().unwrap()
    }

    pub fn plan(self) -> Result<GraphPlan, BuildError> {
        self.plan_with(GraphPlan::default())
    }

    /// Use a preconfigured plan, for example to supply IPC service limits.
    pub fn plan_with(self, mut channels: GraphPlan) -> Result<GraphPlan, BuildError> {
        for task in self.tasks {
            task.register(&mut channels)?;
        }
        Ok(channels)
    }

    pub fn allocate(self) -> Result<GraphStorage, BuildError> {
        self.plan()?.allocate()
    }
}
