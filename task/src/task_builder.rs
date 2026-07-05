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

#[derive(Debug)]
pub struct BuiltTasks {
    pub callbacks: Vec<ConnectedCallback>,
}

#[derive(Debug)]
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
mod test {
    use std::assert_matches;

    use super::*;
    use crate::callback::{GenericCallback, Run};
    use crate::callback_builder::CallbackBuilder;
    use crate::context::Context;
    use crate::generic_publisher::GenericPublisher;
    use crate::generic_subscriber::GenericSubscriber;
    use crate::publisher::{Publisher, PublisherConfig};
    use crate::subscriber::{Subscriber, SubscriberConfig};
    use std::time::Duration;

    /// A generic callback with a configurable number of default subscribers/publishers.
    struct DummyCallback {
        num_subscribers: usize,
        num_publishers: usize,
    }

    impl GenericCallback for DummyCallback {
        fn run_generic(
            &mut self,
            _subscribers: &mut [Box<dyn GenericSubscriber>],
            _publishers: &mut [Box<dyn GenericPublisher>],
            _ctx: &Context,
        ) -> Run {
            Run::new(1)
        }

        fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
            (0..self.num_subscribers)
                .map(|_| {
                    Box::new(Subscriber::<u64>::new(SubscriberConfig {
                        is_optional: false,
                        capacity: 1,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: String::new(),
                    })) as Box<dyn GenericSubscriber>
                })
                .collect()
        }

        fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
            (0..self.num_publishers)
                .map(|_| {
                    Box::new(Publisher::<u64>::new(PublisherConfig {
                        capacity: 1,
                        channel_name: String::new(),
                    })) as Box<dyn GenericPublisher>
                })
                .collect()
        }
    }

    fn make_callback(num_subscribers: usize, num_publishers: usize) -> Box<dyn GenericCallback> {
        Box::new(DummyCallback {
            num_subscribers,
            num_publishers,
        })
    }

    /// A callback that has a single i32 subscriber and no publishers.
    struct I32SubscriberCallback;

    impl GenericCallback for I32SubscriberCallback {
        fn run_generic(
            &mut self,
            _subscribers: &mut [Box<dyn GenericSubscriber>],
            _publishers: &mut [Box<dyn GenericPublisher>],
            _ctx: &Context,
        ) -> Run {
            Run::new(1)
        }

        fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
            vec![Box::new(Subscriber::<i32>::new(SubscriberConfig {
                is_optional: false,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: String::new(),
            }))]
        }

        fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
            vec![]
        }
    }

    fn make_connected_callback(
        name: &str,
        num_subscribers: usize,
        subscriber_channels: &[&str],
        num_publishers: usize,
        publisher_channels: &[&str],
    ) -> ConnectedCallback {
        CallbackBuilder::new(name.into(), make_callback(num_subscribers, num_publishers))
            .with_subscriber_channels(subscriber_channels)
            .with_publisher_channels(publisher_channels)
            .with_execution_duration_callback(|| Duration::from_millis(1))
            .build()
            .unwrap()
    }

    #[test]
    fn empty_build_succeeds() {
        let result = TaskBuilder::new().build();
        assert!(result.is_ok());
        assert!(result.unwrap().callbacks.is_empty());
    }

    #[test]
    fn build_with_one_callback() {
        let callback = make_connected_callback("single", 0, &[], 0, &[]);
        let result = TaskBuilder::new().add_callback(callback).build();

        assert!(result.is_ok());
        let built = result.unwrap();
        assert_eq!(built.callbacks.len(), 1);
        assert_eq!(built.callbacks[0].get_name(), "single");
    }

    #[test]
    fn build_step_can_add_callbacks() {
        struct AddStep;
        impl BuildStep for AddStep {
            fn build_step(
                &self,
                callbacks: &[ConnectedCallback],
            ) -> Result<Vec<ConnectedCallback>, BuildStepError> {
                let name = format!("extra_{}", callbacks.len());
                Ok(vec![make_connected_callback(&name, 0, &[], 0, &[])])
            }
        }

        let result = TaskBuilder::new()
            .add_callback(make_connected_callback("first", 0, &[], 0, &[]))
            .add_build_step(Box::new(AddStep))
            .build();

        assert!(result.is_ok());
        let built = result.unwrap();
        assert_eq!(built.callbacks.len(), 2);
        assert_eq!(built.callbacks[0].get_name(), "first");
        assert_eq!(built.callbacks[1].get_name(), "extra_1");
    }

    #[test]
    fn build_step_error_is_propagated() {
        struct FailingStep;
        impl BuildStep for FailingStep {
            fn build_step(
                &self,
                _callbacks: &[ConnectedCallback],
            ) -> Result<Vec<ConnectedCallback>, BuildStepError> {
                Err("intentional failure".into())
            }
        }

        let result = TaskBuilder::new()
            .add_build_step(Box::new(FailingStep))
            .build();

        assert_matches!(result, Err(TaskBuildError::BuildStepError(_)));
    }

    #[test]
    fn connected_callbacks_matching_types_connect() {
        let producer = make_connected_callback("producer", 0, &[], 1, &["channel"]);
        let consumer = make_connected_callback("consumer", 1, &["channel"], 0, &[]);

        let result = TaskBuilder::new()
            .add_callback(producer)
            .add_callback(consumer)
            .build();

        assert!(result.is_ok(), "matching types should connect");
    }

    #[test]
    fn mismatched_channel_types_fail_to_connect() {
        // Producer publishes u64 on "channel", consumer subscribes i32 on "channel".
        let producer = make_connected_callback("producer", 0, &[], 1, &["channel"]);
        let consumer = CallbackBuilder::new("consumer".into(), Box::new(I32SubscriberCallback))
            .with_subscriber_channels(&["channel"])
            .with_execution_duration_callback(|| Duration::from_millis(1))
            .build()
            .unwrap();

        let result = TaskBuilder::new()
            .add_callback(producer)
            .add_callback(consumer)
            .build();

        assert_matches!(result, Err(TaskBuildError::ConnectionError(_)));
    }

    #[test]
    fn build_with_debug_info_returns_callbacks() {
        let callback = make_connected_callback("debug", 0, &[], 0, &[]);
        let result = TaskBuilder::new()
            .add_callback(callback)
            .build_with_debug_info();

        assert!(result.is_ok());
        let built = result.unwrap();
        assert_eq!(built.callbacks.len(), 1);
        assert_eq!(built.callbacks[0].get_name(), "debug");
    }
}
