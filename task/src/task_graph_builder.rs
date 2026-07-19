use std::{collections::HashMap, fmt};

use crate::{
    callback::{CallbackNode, MismatchTypeError, connect_callback_nodes},
    callback_builder::CallbackBuilder,
    pub_sub::{CallbackNodeName, ChannelName},
};

pub type TaskGraphBuildStepError = Box<dyn std::error::Error>;

/// Allows for introspection on the entire set of callback nodes so that new callback nodes can be
/// added that are derived from the existing callback node set.
/// For example, we can add logging or diagnostic callback nodes that introspect based on existing
/// publishers.
/// These are run in a predefined order and will be sensitive to the ordering they're run in.
/// It is possible to run a given step multiple times. For example, if we run logging, then
/// diagnostics, then logging again, we're able to log the diagnostic channels as well as handle
/// diagnostics of the logging. However, this can be tricky to reason about.
pub trait TaskGraphBuildStep {
    /// Human-readable name for this build step, used in error messages.
    fn name(&self) -> &str;

    /// Exposes access to all existing callback nodes.
    ///
    /// Allows the step to return additional callback nodes to add, if desired.
    fn build_step(
        &self,
        nodes: &[CallbackNode],
    ) -> Result<Vec<CallbackNode>, TaskGraphBuildStepError>;
}

pub struct TaskGraphBuilder {
    nodes: Vec<CallbackNode>,
    pending_builders: Vec<CallbackBuilder>,
    build_steps: Vec<Box<dyn TaskGraphBuildStep>>,
}

#[derive(Debug)]
pub struct BuiltTaskGraph {
    pub nodes: Vec<CallbackNode>,
}

#[derive(Debug)]
pub struct BuiltTaskGraphWithDebugInfo {
    pub nodes: Vec<CallbackNode>,
    pub dangling_subscribers: Vec<ChannelName>,
    pub dangling_publishers: Vec<ChannelName>,
}

#[derive(Debug)]
pub enum TaskGraphBuildError {
    /// Error hit during callback node connection.
    ConnectionError(MismatchTypeError),
    /// Error hit during a graph build step.
    BuildStepError {
        /// Name of the build step that failed.
        step_name: String,
        /// The error returned by the build step.
        error: TaskGraphBuildStepError,
    },
    /// Error hit building a callback node from a queued [`CallbackBuilder`].
    CallbackBuildError {
        /// Name of the callback node that failed to build.
        callback_name: CallbackNodeName,
        /// The error returned by [`CallbackBuilder::build`].
        error: crate::callback_builder::CallbackBuildError,
    },
}

impl fmt::Display for TaskGraphBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionError(e) => write!(f, "{}", e),
            Self::BuildStepError { step_name, error } => {
                write!(f, "Build step '{step_name}' failed with {}", error)
            }
            Self::CallbackBuildError {
                callback_name,
                error,
            } => {
                write!(
                    f,
                    "Callback node '{}' build failed with {}",
                    callback_name, error
                )
            }
        }
    }
}

impl Default for TaskGraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn find_dangling_subscribers(nodes: &[CallbackNode]) -> Vec<ChannelName> {
    let mut channel_to_subscriber_count = HashMap::<&str, usize>::new();

    for node in nodes {
        for input in node.subscribers().iter() {
            let channel = input.config().channel_name.as_str();
            *channel_to_subscriber_count.entry(channel).or_default() += 1;
        }
    }

    channel_to_subscriber_count
        .into_iter()
        .filter(|(_, count)| *count == 0)
        .map(|(channel, _)| channel.to_owned())
        .collect()
}

fn find_dangling_publishers(nodes: &[CallbackNode]) -> Vec<ChannelName> {
    let mut channel_to_publisher_count = HashMap::<&str, usize>::new();

    for node in nodes {
        for input in node.publishers().iter() {
            let channel = input.config().channel_name.as_str();
            *channel_to_publisher_count.entry(channel).or_default() += 1;
        }
    }

    channel_to_publisher_count
        .into_iter()
        .filter(|(_, count)| *count == 0)
        .map(|(channel, _)| channel.to_owned())
        .collect()
}

impl TaskGraphBuilder {
    pub fn new() -> TaskGraphBuilder {
        TaskGraphBuilder {
            nodes: vec![],
            pending_builders: vec![],
            build_steps: vec![],
        }
    }

    /// Add an already-built callback node to the task graph.
    pub fn add_callback(mut self, callback: CallbackNode) -> TaskGraphBuilder {
        self.nodes.push(callback);
        self
    }

    /// Add a [`CallbackBuilder`] to the task graph. Its `.build()` will be called when the task
    /// graph is built, so any error surfaces as a [`TaskGraphBuildError::CallbackBuildError`].
    pub fn add_callback_builder(mut self, builder: CallbackBuilder) -> TaskGraphBuilder {
        self.pending_builders.push(builder);
        self
    }

    pub fn add_build_step(mut self, build_step: Box<dyn TaskGraphBuildStep>) -> TaskGraphBuilder {
        self.build_steps.push(build_step);
        self
    }

    /// Builds any queued [`CallbackBuilder`]s, runs build steps in the order they were added,
    /// and then connects all callback nodes.
    pub fn build(mut self) -> Result<BuiltTaskGraph, TaskGraphBuildError> {
        // Build any callback builders that were queued before running graph-level build steps.
        for builder in self.pending_builders.drain(..) {
            let callback_name = builder.name().to_owned();
            let node =
                builder
                    .build()
                    .map_err(|error| TaskGraphBuildError::CallbackBuildError {
                        callback_name,
                        error,
                    })?;
            self.nodes.push(node);
        }

        // Run all build steps
        for step in self.build_steps.drain(..) {
            let step_name = step.name();
            let mut additional_nodes = step.build_step(&self.nodes).map_err(|error| {
                TaskGraphBuildError::BuildStepError {
                    step_name: step_name.to_owned(),
                    error,
                }
            })?;
            self.nodes.append(&mut additional_nodes);
        }

        connect_callback_nodes(&mut self.nodes).map_err(TaskGraphBuildError::ConnectionError)?;

        Ok(BuiltTaskGraph { nodes: self.nodes })
    }

    pub fn build_with_debug_info(self) -> Result<BuiltTaskGraphWithDebugInfo, TaskGraphBuildError> {
        let built_graph = self.build()?;

        let dangling_subscribers = find_dangling_subscribers(&built_graph.nodes);
        let dangling_publishers = find_dangling_publishers(&built_graph.nodes);
        Ok(BuiltTaskGraphWithDebugInfo {
            nodes: built_graph.nodes,
            dangling_subscribers,
            dangling_publishers,
        })
    }
}

#[cfg(test)]
mod test {
    use std::assert_matches;

    use super::*;
    use crate::callback::{Callback, Run};
    use crate::callback_builder::CallbackBuilder;
    use crate::context::Context;
    use crate::generic_publisher::GenericPublisher;
    use crate::generic_subscriber::GenericSubscriber;
    use crate::publisher::{Publisher, PublisherConfig};
    use crate::subscriber::{Subscriber, SubscriberConfig};
    use std::time::Duration;

    /// A callback with a configurable number of default subscribers/publishers.
    struct DummyCallback {
        num_subscribers: usize,
        num_publishers: usize,
    }

    impl Callback for DummyCallback {
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

    fn make_callback(num_subscribers: usize, num_publishers: usize) -> Box<dyn Callback> {
        Box::new(DummyCallback {
            num_subscribers,
            num_publishers,
        })
    }

    /// A callback that has a single i32 subscriber and no publishers.
    struct I32SubscriberCallback;

    impl Callback for I32SubscriberCallback {
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

    fn make_callback_node(
        name: &str,
        num_subscribers: usize,
        subscriber_channels: &[&str],
        num_publishers: usize,
        publisher_channels: &[&str],
    ) -> CallbackNode {
        CallbackBuilder::new(name.into(), make_callback(num_subscribers, num_publishers))
            .with_subscriber_channels(subscriber_channels)
            .with_publisher_channels(publisher_channels)
            .with_execution_duration_callback(|| Duration::from_millis(1))
            .build()
            .unwrap()
    }

    #[test]
    fn empty_build_succeeds() {
        let result = TaskGraphBuilder::new().build();
        assert!(result.is_ok());
        assert!(result.unwrap().nodes.is_empty());
    }

    #[test]
    fn build_with_one_callback() {
        let callback = make_callback_node("single", 0, &[], 0, &[]);
        let result = TaskGraphBuilder::new().add_callback(callback).build();

        assert!(result.is_ok());
        let built = result.unwrap();
        assert_eq!(built.nodes.len(), 1);
        assert_eq!(built.nodes[0].name(), "single");
    }

    #[test]
    fn build_step_can_add_callbacks() {
        struct AddStep;
        impl TaskGraphBuildStep for AddStep {
            fn name(&self) -> &str {
                "add-step"
            }

            fn build_step(
                &self,
                nodes: &[CallbackNode],
            ) -> Result<Vec<CallbackNode>, TaskGraphBuildStepError> {
                let name = format!("extra_{}", nodes.len());
                Ok(vec![make_callback_node(&name, 0, &[], 0, &[])])
            }
        }

        let result = TaskGraphBuilder::new()
            .add_callback(make_callback_node("first", 0, &[], 0, &[]))
            .add_build_step(Box::new(AddStep))
            .build();

        assert!(result.is_ok());
        let built = result.unwrap();
        assert_eq!(built.nodes.len(), 2);
        assert_eq!(built.nodes[0].name(), "first");
        assert_eq!(built.nodes[1].name(), "extra_1");
    }

    #[test]
    fn build_step_error_is_propagated() {
        struct FailingStep;
        impl TaskGraphBuildStep for FailingStep {
            fn name(&self) -> &str {
                "failing-step"
            }

            fn build_step(
                &self,
                _nodes: &[CallbackNode],
            ) -> Result<Vec<CallbackNode>, TaskGraphBuildStepError> {
                Err("intentional failure".into())
            }
        }

        let result = TaskGraphBuilder::new()
            .add_build_step(Box::new(FailingStep))
            .build();

        assert_matches!(result, Err(TaskGraphBuildError::BuildStepError { .. }));
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failing-step"),
            "error should mention step name: {err}"
        );
        assert!(
            err.contains("intentional failure"),
            "error should mention underlying error: {err}"
        );
    }

    #[test]
    fn callback_build_error_is_propagated() {
        let result = TaskGraphBuilder::new()
            .add_callback_builder(CallbackBuilder::new(
                "BadDuration".into(),
                make_callback(0, 0),
            ))
            .build();

        assert_matches!(result, Err(TaskGraphBuildError::CallbackBuildError { .. }));
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("BadDuration"),
            "error should mention callback name: {err}"
        );
    }

    #[test]
    fn connected_callbacks_matching_types_connect() {
        let producer = make_callback_node("producer", 0, &[], 1, &["channel"]);
        let consumer = make_callback_node("consumer", 1, &["channel"], 0, &[]);

        let result = TaskGraphBuilder::new()
            .add_callback(producer)
            .add_callback(consumer)
            .build();

        assert!(result.is_ok(), "matching types should connect");
    }

    #[test]
    fn mismatched_channel_types_fail_to_connect() {
        // Producer publishes u64 on "channel", consumer subscribes i32 on "channel".
        let producer = make_callback_node("producer", 0, &[], 1, &["channel"]);
        let consumer = CallbackBuilder::new("consumer".into(), Box::new(I32SubscriberCallback))
            .with_subscriber_channels(&["channel"])
            .with_execution_duration_callback(|| Duration::from_millis(1))
            .build()
            .unwrap();

        let result = TaskGraphBuilder::new()
            .add_callback(producer)
            .add_callback(consumer)
            .build();

        assert_matches!(result, Err(TaskGraphBuildError::ConnectionError(_)));
    }

    #[test]
    fn build_with_debug_info_returns_callbacks() {
        let callback = make_callback_node("debug", 0, &[], 0, &[]);
        let result = TaskGraphBuilder::new()
            .add_callback(callback)
            .build_with_debug_info();

        assert!(result.is_ok());
        let built = result.unwrap();
        assert_eq!(built.nodes.len(), 1);
        assert_eq!(built.nodes[0].name(), "debug");
    }
}
