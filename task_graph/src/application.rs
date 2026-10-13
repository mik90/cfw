//! Declaration-driven construction with externally owned channel storage.
use std::cell::RefCell;

use crate::{
    BuildError, BuiltGraph, CallbackSchedule, NamedBindings, NamedPlan, NamedStorage,
    RegisteredTask, Task, TaskRegistration, Topology,
};

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

    pub fn plan(self) -> Result<ApplicationPlan, BuildError> {
        self.plan_with(NamedPlan::default())
    }

    /// Use a preconfigured plan, for example to supply IPC service limits.
    pub fn plan_with(self, mut channels: NamedPlan) -> Result<ApplicationPlan, BuildError> {
        let tasks = self
            .tasks
            .into_iter()
            .map(|task| task.register(&mut channels))
            .collect::<Result<_, _>>()?;
        Ok(ApplicationPlan { channels, tasks })
    }

    pub fn allocate(self) -> Result<ApplicationStorage, BuildError> {
        self.plan()?.allocate()
    }
}

/// Resolved task declarations, before allocation of recording/replay endpoints.
pub struct ApplicationPlan {
    channels: NamedPlan,
    tasks: Vec<RegisteredTask>,
}

impl ApplicationPlan {
    pub fn channels_mut(&mut self) -> &mut NamedPlan {
        &mut self.channels
    }

    pub fn allocate(self) -> Result<ApplicationStorage, BuildError> {
        Ok(ApplicationStorage {
            storage: self.channels.allocate()?,
            tasks: RefCell::new(Some(self.tasks)),
        })
    }
}

/// Owns storage independently of the single-use graph definition.
/// Graphs, endpoints, and retained messages borrow this owner.
///
/// ```compile_fail,E0597
/// use task_graph::ApplicationBuilder;
/// let graph = {
///     let storage = ApplicationBuilder::new().allocate().unwrap();
///     storage.build().unwrap()
/// };
/// drop(graph);
/// ```
pub struct ApplicationStorage {
    storage: NamedStorage,
    tasks: RefCell<Option<Vec<RegisteredTask>>>,
}

impl ApplicationStorage {
    pub fn topology(&self) -> &Topology {
        self.storage.topology()
    }

    pub fn build(&self) -> Result<BuiltGraph<'_>, BuildError> {
        Ok(self.build_with_bindings()?.0)
    }

    /// Return bindings for planned captures, replay sources, and test fixtures.
    pub fn build_with_bindings(&self) -> Result<(BuiltGraph<'_>, NamedBindings<'_>), BuildError> {
        let bindings = self.bind()?;
        let graph = self.build_with(&bindings)?;
        Ok((graph, bindings))
    }

    pub fn bind(&self) -> Result<NamedBindings<'_>, BuildError> {
        self.storage.bind()
    }

    /// Build after configuring publisher observers or replay hydration on bindings.
    pub fn build_with<'a>(
        &'a self,
        bindings: &NamedBindings<'a>,
    ) -> Result<BuiltGraph<'a>, BuildError> {
        let tasks = self
            .tasks
            .borrow_mut()
            .take()
            .ok_or_else(|| BuildError("application has already been built".into()))?;
        let mut graph = self.storage.graph_builder();
        for task in tasks {
            task.add_to_graph(&mut graph, bindings);
        }
        let graph = graph.build().map_err(|e| BuildError(format!("{e:?}")))?;
        Ok(graph)
    }
}
