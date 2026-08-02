use std::{collections::HashMap, fmt};

use crate::{
    ChannelRegistry,
    callback::{CallbackNode, MismatchTypeError, connect_callback_nodes},
    callback_builder::CallbackBuilder,
    execution_log,
    executor::ThreadPoolConfig,
    pub_sub::{CallbackNodeName, ChannelName},
    publisher::Publisher,
};

pub type TaskGraphBuildStepError = Box<dyn std::error::Error>;

/// Accumulates callback nodes that will be assigned to one executor thread pool.
/// Constructed via [`TaskGraphBuilder::add_pool`].
pub struct PoolBuilder {
    thread_count: usize,
    callbacks: Vec<CallbackNode>,
    pending_builders: Vec<CallbackBuilder>,
}

impl PoolBuilder {
    pub fn add_callback(mut self, callback: CallbackNode) -> Self {
        self.callbacks.push(callback);
        self
    }

    pub fn add_callback_builder(mut self, builder: CallbackBuilder) -> Self {
        self.pending_builders.push(builder);
        self
    }
}

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
    pools: Vec<PoolBuilder>,
    build_steps: Vec<Box<dyn TaskGraphBuildStep>>,
    log_executions: bool,
    channel_registry: ChannelRegistry,
}

pub struct BuiltTaskGraph {
    pub pools: Vec<ThreadPoolConfig>,
    /// Per-worker execution-log publishers, wired to any execution-log
    /// subscribers found in the graph. Empty when execution logging is off.
    pub execution_log_publishers: Vec<Publisher<execution_log::ExecutionLogMessage>>,
}

impl fmt::Debug for BuiltTaskGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltTaskGraph")
            .field("pools", &self.pools)
            .field(
                "execution_log_publishers",
                &self.execution_log_publishers.len(),
            )
            .finish()
    }
}

impl BuiltTaskGraph {
    pub fn print(&self) {
        for (i, pool) in self.pools.iter().enumerate() {
            println!("Pool {i} ({} threads):", pool.thread_count);
            for node in &pool.nodes {
                println!("  {node}");
            }
        }
    }
}

pub struct BuiltTaskGraphWithDebugInfo {
    pub pools: Vec<ThreadPoolConfig>,
    pub execution_log_publishers: Vec<Publisher<execution_log::ExecutionLogMessage>>,
    pub dangling_subscribers: Vec<ChannelName>,
    pub dangling_publishers: Vec<ChannelName>,
}

impl fmt::Debug for BuiltTaskGraphWithDebugInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltTaskGraphWithDebugInfo")
            .field("pools", &self.pools)
            .field(
                "execution_log_publishers",
                &self.execution_log_publishers.len(),
            )
            .field("dangling_subscribers", &self.dangling_subscribers)
            .field("dangling_publishers", &self.dangling_publishers)
            .finish()
    }
}

#[derive(Debug)]
/// Error hit during callback node connection.
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
    /// Error during execution-log wiring.
    ExecutionLogError(execution_log::ExecutionLogConnectError),
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
            Self::ExecutionLogError(e) => write!(f, "Execution log wiring failed: {}", e),
        }
    }
}

impl Default for TaskGraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn find_dangling_subscribers(nodes: &[&CallbackNode]) -> Vec<ChannelName> {
    let mut channel_to_subscriber_count = HashMap::<&str, usize>::new();

    for node in nodes {
        node.callback().for_each_subscriber(&mut |s| {
            let channel = s.config().channel_name.as_str();
            *channel_to_subscriber_count.entry(channel).or_default() += 1;
        });
    }

    channel_to_subscriber_count
        .into_iter()
        .filter(|(_, count)| *count == 0)
        .map(|(channel, _)| channel.to_owned())
        .collect()
}

fn find_dangling_publishers(nodes: &[&CallbackNode]) -> Vec<ChannelName> {
    let mut channel_to_publisher_count = HashMap::<&str, usize>::new();

    for node in nodes {
        node.callback().for_each_publisher(&mut |p| {
            let channel = p.config().channel_name.as_str();
            *channel_to_publisher_count.entry(channel).or_default() += 1;
        });
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
            pools: vec![],
            build_steps: vec![],
            log_executions: false,
            channel_registry: ChannelRegistry::new(),
        }
    }

    /// Set to `true` to opt every callback node in the graph into execution
    /// logging. Defaults to `false`. Applied after build steps run, so
    /// build-step-created nodes are included.
    pub fn with_log_executions(mut self, enabled: bool) -> TaskGraphBuilder {
        self.log_executions = enabled;
        self
    }

    /// Add a thread pool to the graph. The closure receives a fresh [`PoolBuilder`]
    /// and must return it after adding any callbacks to be assigned to this pool.
    /// Build-step-created nodes are assigned to the first pool.
    pub fn add_pool(
        mut self,
        thread_count: usize,
        configure: impl FnOnce(PoolBuilder) -> PoolBuilder,
    ) -> TaskGraphBuilder {
        let pool = PoolBuilder {
            thread_count,
            callbacks: Vec::new(),
            pending_builders: Vec::new(),
        };
        self.pools.push(configure(pool));
        self
    }

    pub fn add_build_step(mut self, build_step: Box<dyn TaskGraphBuildStep>) -> TaskGraphBuilder {
        self.build_steps.push(build_step);
        self
    }

    pub fn build(mut self) -> Result<BuiltTaskGraph, TaskGraphBuildError> {
        let pool_thread_counts: Vec<usize> = self.pools.iter().map(|p| p.thread_count).collect();
        let mut all_nodes: Vec<CallbackNode> = Vec::new();
        let mut pool_node_counts: Vec<usize> = Vec::with_capacity(self.pools.len());

        for mut pool in self.pools {
            let start = all_nodes.len();
            for builder in pool.pending_builders.drain(..) {
                let callback_name = builder.name().to_owned();
                let node =
                    builder
                        .build()
                        .map_err(|error| TaskGraphBuildError::CallbackBuildError {
                            callback_name,
                            error,
                        })?;
                all_nodes.push(node);
            }
            all_nodes.append(&mut pool.callbacks);
            pool_node_counts.push(all_nodes.len() - start);
        }

        for step in self.build_steps.drain(..) {
            let step_name = step.name();
            let mut additional_nodes = step.build_step(&all_nodes).map_err(|error| {
                TaskGraphBuildError::BuildStepError {
                    step_name: step_name.to_owned(),
                    error,
                }
            })?;
            all_nodes.append(&mut additional_nodes);
        }

        if all_nodes.is_empty() {
            return Ok(BuiltTaskGraph {
                pools: vec![],
                execution_log_publishers: vec![],
            });
        }

        let total_original: usize = pool_node_counts.iter().sum();
        let extra = all_nodes.len() - total_original;
        if pool_node_counts.is_empty() {
            if self.log_executions {
                for node in all_nodes.iter_mut() {
                    node.set_log_executions(true);
                }
            }
            connect_callback_nodes(&mut all_nodes).map_err(TaskGraphBuildError::ConnectionError)?;
            return Ok(BuiltTaskGraph {
                pools: vec![ThreadPoolConfig::new(1, all_nodes)],
                execution_log_publishers: vec![],
            });
        }
        pool_node_counts[0] += extra;

        if self.log_executions {
            for node in all_nodes.iter_mut() {
                node.set_log_executions(true);
            }
        }

        connect_callback_nodes(&mut all_nodes).map_err(TaskGraphBuildError::ConnectionError)?;

        let mut pools = Vec::with_capacity(pool_node_counts.len());
        for (i, &count) in pool_node_counts.iter().enumerate() {
            let nodes: Vec<CallbackNode> = all_nodes.drain(..count).collect();
            pools.push(ThreadPoolConfig::new(pool_thread_counts[i], nodes));
        }

        let execution_log_publishers = if self.log_executions {
            let mut log_pubs = execution_log::log_publishers(&pools);
            execution_log::connect(&mut pools, &mut log_pubs)
                .map_err(TaskGraphBuildError::ExecutionLogError)?;
            log_pubs
        } else {
            vec![]
        };

        Ok(BuiltTaskGraph {
            pools,
            execution_log_publishers,
        })
    }

    pub fn build_with_debug_info(self) -> Result<BuiltTaskGraphWithDebugInfo, TaskGraphBuildError> {
        let built_graph = self.build()?;

        let all_nodes: Vec<&CallbackNode> = built_graph
            .pools
            .iter()
            .flat_map(|p| p.nodes.iter())
            .collect();
        let dangling_subscribers = find_dangling_subscribers(&all_nodes);
        let dangling_publishers = find_dangling_publishers(&all_nodes);
        Ok(BuiltTaskGraphWithDebugInfo {
            pools: built_graph.pools,
            execution_log_publishers: built_graph.execution_log_publishers,
            dangling_subscribers,
            dangling_publishers,
        })
    }
}

#[cfg(test)]
mod test {
    use std::assert_matches;

    use super::*;
    use crate::callback::{Callback, PortMut, Run};
    use crate::callback_builder::CallbackBuilder;
    use crate::context::Context;
    use crate::generic_publisher::GenericPublisher;
    use crate::generic_subscriber::GenericSubscriber;
    use crate::publisher::{Publisher, PublisherConfig};
    use crate::subscriber::{Subscriber, SubscriberConfig};
    use std::time::Duration;

    struct DummyCallback {
        subs: Vec<Box<dyn GenericSubscriber>>,
        pubs: Vec<Box<dyn GenericPublisher>>,
    }

    impl Callback for DummyCallback {
        fn run(&mut self, _ctx: &Context) -> Run {
            Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            for s in &self.subs {
                f(s.as_ref());
            }
        }
        fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
            for p in &self.pubs {
                f(p.as_ref());
            }
        }
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            for s in self.subs.iter_mut() {
                f(s.as_mut());
            }
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
            for p in self.pubs.iter_mut() {
                f(p.as_mut());
            }
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            for s in self.subs.iter_mut() {
                f(PortMut::Subscriber(s.as_mut()));
            }
            for p in self.pubs.iter_mut() {
                f(PortMut::Publisher(p.as_mut()));
            }
        }
    }

    fn make_callback(num_subscribers: usize, num_publishers: usize) -> Box<dyn Callback> {
        Box::new(DummyCallback {
            subs: (0..num_subscribers)
                .map(|_| {
                    Box::new(Subscriber::<u64>::new(SubscriberConfig {
                        is_optional: false,
                        capacity: 1,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: String::new(),
                    })) as Box<dyn GenericSubscriber>
                })
                .collect(),
            pubs: (0..num_publishers)
                .map(|_| {
                    Box::new(Publisher::<u64>::new(PublisherConfig {
                        capacity: 1,
                        channel_name: String::new(),
                    })) as Box<dyn GenericPublisher>
                })
                .collect(),
        })
    }

    struct I32SubscriberCallback {
        subscriber: Subscriber<i32>,
    }

    impl Callback for I32SubscriberCallback {
        fn run(&mut self, _ctx: &Context) -> Run {
            Run::new(1)
        }
        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            f(&self.subscriber);
        }
        fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            f(&mut self.subscriber);
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            f(PortMut::Subscriber(&mut self.subscriber));
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
        assert!(result.unwrap().pools.is_empty());
    }

    #[test]
    fn build_with_one_callback() {
        let callback = make_callback_node("single", 0, &[], 0, &[]);
        let result = TaskGraphBuilder::new()
            .add_pool(1, |p| p.add_callback(callback))
            .build();

        assert!(result.is_ok());
        let built = result.unwrap();
        assert_eq!(built.pools.len(), 1);
        assert_eq!(built.pools[0].thread_count, 1);
        assert_eq!(built.pools[0].nodes.len(), 1);
        assert_eq!(built.pools[0].nodes[0].name(), "single");
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
            .add_pool(1, |p| {
                p.add_callback(make_callback_node("first", 0, &[], 0, &[]))
            })
            .add_build_step(Box::new(AddStep))
            .build();

        assert!(result.is_ok());
        let built = result.unwrap();
        assert_eq!(built.pools.len(), 1);
        assert_eq!(built.pools[0].nodes.len(), 2);
        assert_eq!(built.pools[0].nodes[0].name(), "first");
        assert_eq!(built.pools[0].nodes[1].name(), "extra_1");
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
            .add_pool(1, |p| {
                p.add_callback_builder(CallbackBuilder::new(
                    "BadDuration".into(),
                    make_callback(0, 0),
                ))
            })
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
            .add_pool(1, |p| p.add_callback(producer).add_callback(consumer))
            .build();

        assert!(result.is_ok(), "matching types should connect");
    }

    #[test]
    fn mismatched_channel_types_fail_to_connect() {
        let producer = make_callback_node("producer", 0, &[], 1, &["channel"]);
        let consumer = CallbackBuilder::new(
            "consumer".into(),
            Box::new(I32SubscriberCallback {
                subscriber: Subscriber::<i32>::new(SubscriberConfig {
                    is_optional: false,
                    capacity: 1,
                    is_trigger: true,
                    keep_across_runs: true,
                    channel_name: String::new(),
                }),
            }),
        )
        .with_subscriber_channels(&["channel"])
        .with_execution_duration_callback(|| Duration::from_millis(1))
        .build()
        .unwrap();

        let result = TaskGraphBuilder::new()
            .add_pool(1, |p| p.add_callback(producer).add_callback(consumer))
            .build();

        assert_matches!(result, Err(TaskGraphBuildError::ConnectionError(_)));
    }

    #[test]
    fn build_with_debug_info_returns_callbacks() {
        let callback = make_callback_node("debug", 0, &[], 0, &[]);
        let result = TaskGraphBuilder::new()
            .add_pool(1, |p| p.add_callback(callback))
            .build_with_debug_info();

        assert!(result.is_ok());
        let built = result.unwrap();
        assert_eq!(built.pools.len(), 1);
        assert_eq!(built.pools[0].nodes.len(), 1);
        assert_eq!(built.pools[0].nodes[0].name(), "debug");
    }
}
