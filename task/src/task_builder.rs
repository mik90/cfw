use std::{collections::HashMap, fmt};

use crate::{
    callback::{ConnectedCallback, MismatchTypeError, connect_callbacks},
    pub_sub::ChannelName,
};

pub type BuildStepError = Box<dyn std::error::Error>;

/// Allows for introspection on entire set of tasks so that new tasks can be added that are derived from the existing task set.
/// For example, we can add logging or diagnostic tasks that introspect based on existing publishers.
/// These are run in a predefined order and will be sensitive to the ordering they're run it.
/// It is possible to run a given step multiple times. For example, if we run logging, then diagnostics, then logging again,
/// we're able to log the diagnostic channels as well as handle diagnostics of the logging. However, this can be tricky to reason about.
pub trait BuildStep {
    /// Exposes access to all existing callbacks.
    ///
    /// Allows step to return additional callbacks to add, if desired.
    fn build_step(
        &self,
        callbacks: &[ConnectedCallback],
    ) -> Result<Vec<ConnectedCallback>, BuildStepError>;
}

pub struct TaskBuilder {
    callbacks: Vec<ConnectedCallback>,
    build_steps: Vec<Box<dyn BuildStep>>,
}

pub struct BuiltTasks {
    pub callbacks: Vec<ConnectedCallback>,
}

pub struct BuiltTasksWithDebugInfo {
    pub callbacks: Vec<ConnectedCallback>,
    pub dangling_subscribers: Vec<ChannelName>,
    pub dangling_publishers: Vec<ChannelName>,
}

#[derive(Debug)]
pub enum TaskBuildError {
    ConnectionError(MismatchTypeError), // Error hit during callback connection
    BuildStepError(BuildStepError),     // More generic error hit during build step
}

impl fmt::Display for TaskBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionError(e) => write!(f, "{}", e),
            Self::BuildStepError(e) => write!(f, "Build step failed with {}", e),
        }
    }
}

impl Default for TaskBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn find_dangling_subscribers(callbacks: &[ConnectedCallback]) -> Vec<ChannelName> {
    let mut channel_to_subscriber_count = HashMap::<&str, usize>::new();

    for callback in callbacks {
        for input in callback.get_subscribers().iter() {
            let channel = input.get_config().channel_name.as_str();
            *channel_to_subscriber_count.entry(channel).or_default() += 1;
        }
    }

    channel_to_subscriber_count
        .into_iter()
        .filter(|(_, count)| *count == 0)
        .map(|(channel, _)| channel.to_owned())
        .collect()
}

fn find_dangling_publishers(callbacks: &[ConnectedCallback]) -> Vec<ChannelName> {
    let mut channel_to_publisher_count = HashMap::<&str, usize>::new();

    for callback in callbacks {
        for input in callback.get_publishers().iter() {
            let channel = input.get_config().channel_name.as_str();
            *channel_to_publisher_count.entry(channel).or_default() += 1;
        }
    }

    channel_to_publisher_count
        .into_iter()
        .filter(|(_, count)| *count == 0)
        .map(|(channel, _)| channel.to_owned())
        .collect()
}

impl TaskBuilder {
    pub fn new() -> TaskBuilder {
        TaskBuilder {
            callbacks: vec![],
            build_steps: vec![],
        }
    }

    pub fn add_callback(mut self, callback: ConnectedCallback) -> TaskBuilder {
        self.callbacks.push(callback);
        self
    }

    pub fn add_build_step(mut self, build_step: Box<dyn BuildStep>) -> TaskBuilder {
        self.build_steps.push(build_step);
        self
    }

    /// Runs build steps in the order they were added, and then connects all callbacks.
    pub fn build(mut self) -> Result<BuiltTasks, TaskBuildError> {
        // Run all build steps
        for step in self.build_steps.drain(..) {
            let mut additional_callbacks = step
                .build_step(&self.callbacks)
                .map_err(TaskBuildError::BuildStepError)?;
            self.callbacks.append(&mut additional_callbacks);
        }

        connect_callbacks(&mut self.callbacks).map_err(TaskBuildError::ConnectionError)?;

        Ok(BuiltTasks {
            callbacks: self.callbacks,
        })
    }

    pub fn build_with_debug_info(self) -> Result<BuiltTasksWithDebugInfo, TaskBuildError> {
        let built_tasks = self.build()?;

        let dangling_subscribers = find_dangling_subscribers(&built_tasks.callbacks);
        let dangling_publishers = find_dangling_publishers(&built_tasks.callbacks);
        Ok(BuiltTasksWithDebugInfo {
            callbacks: built_tasks.callbacks,
            dangling_subscribers,
            dangling_publishers,
        })
    }
}

#[cfg(test)]
mod test {}
