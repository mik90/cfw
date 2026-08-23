use std::{collections::HashMap, fmt};

use crate::{
    ChannelRegistry,
    callback::{CallbackNode, MismatchTypeError, connect_callback_nodes},
    callback_builder::CallbackBuilder,
    execution_log::{self, ExecutionLogLevel},
    executor::ThreadPoolConfig,
    pub_sub::{CallbackNameTag, CallbackNodeName, ChannelName, ChannelNameTag},
    publisher::Publisher,
    string_interner::StringInterner,
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
        channel_registry: &mut ChannelRegistry,
    ) -> Result<Vec<CallbackNode>, TaskGraphBuildStepError>;
}

pub struct TaskGraphBuilder {
    pools: Vec<PoolBuilder>,
    build_steps: Vec<Box<dyn TaskGraphBuildStep>>,
    execution_log_level_override: Option<ExecutionLogLevel>,
    channel_registry: ChannelRegistry,
    debug_info: bool,
}

pub struct BuiltTaskGraph {
    pub pools: Vec<ThreadPoolConfig>,
    /// Per-worker execution-log publishers, wired to any execution-log
    /// subscribers found in the graph. Empty when execution logging is off.
    pub execution_log_publishers: Vec<Publisher<execution_log::ExecutionLogMessage>>,
    /// Channel registry populated with every loggable channel found across the
    /// graph's publishers/subscribers (plus the framework's execution-log
    /// channel). Replay executors consume this to deserialize logged messages.
    pub channel_registry: ChannelRegistry,
    /// All channel names found at end of graph build
    pub channel_interner: StringInterner<ChannelNameTag>,
    /// All callback names found at end of graph build
    pub callback_interner: StringInterner<CallbackNameTag>,
    /// Dangling-channel diagnostics, present when built with
    /// [`TaskGraphBuilder::with_debug_info`].
    pub debug_info: Option<GraphDebugInfo>,
}

/// Dangling-channel diagnostics, produced when built with
/// [`TaskGraphBuilder::with_debug_info`].
pub struct GraphDebugInfo {
    pub dangling_subscribers: Vec<ChannelName>,
    pub dangling_publishers: Vec<ChannelName>,
}

impl fmt::Debug for BuiltTaskGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltTaskGraph")
            .field("pools", &self.pools)
            .field(
                "execution_log_publishers",
                &self.execution_log_publishers.len(),
            )
            .field(
                "channel_registry",
                &self.channel_registry.serializer_count(),
            )
            .field("debug_info", &self.debug_info)
            .finish()
    }
}

impl fmt::Debug for GraphDebugInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GraphDebugInfo")
            .field("dangling_subscribers", &self.dangling_subscribers)
            .field("dangling_publishers", &self.dangling_publishers)
            .finish()
    }
}

impl BuiltTaskGraph {
    pub fn print(&self) {
        for (i, pool) in self.pools.iter().enumerate() {
            println!("Pool {i} ({} threads):", pool.thread_count);
            for node in pool.nodes.iter_shared() {
                // Build time: the graph is not shared with any worker thread
                // yet, so access cannot conflict.
                node.access(|node| println!("  {node}"));
            }
        }
    }
}

impl Drop for BuiltTaskGraph {
    fn drop(&mut self) {
        // Subscriber buffers hold ArenaPtrs into arenas owned by the nodes'
        // publishers and by `execution_log_publishers` (declared after
        // `pools`, so they outlive this). Clear every subscriber buffer before
        // any of those arenas drop; without this, a subscriber queue's drop
        // glue would dereference freed arena slots. Idempotent with each
        // storage's own Drop.
        for pool in &self.pools {
            pool.nodes.cleanup_subscribers();
        }
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

fn find_dangling_subscribers(pools: &[ThreadPoolConfig]) -> Vec<ChannelName> {
    let mut channel_to_subscriber_count = HashMap::<ChannelName, usize>::new();

    for pool in pools {
        for node in pool.nodes.iter_shared() {
            node.access(|node| {
                node.callback().for_each_subscriber(&mut |s| {
                    let channel = s.config().channel_name.clone();
                    *channel_to_subscriber_count.entry(channel).or_default() += 1;
                });
            });
        }
    }

    channel_to_subscriber_count
        .into_iter()
        .filter(|(_, count)| *count == 0)
        .map(|(channel, _)| channel)
        .collect()
}

fn find_dangling_publishers(pools: &[ThreadPoolConfig]) -> Vec<ChannelName> {
    let mut channel_to_publisher_count = HashMap::<ChannelName, usize>::new();

    for pool in pools {
        for node in pool.nodes.iter_shared() {
            node.access(|node| {
                node.callback().for_each_publisher(&mut |p| {
                    let channel = p.config().channel_name.clone();
                    *channel_to_publisher_count.entry(channel).or_default() += 1;
                });
            });
        }
    }

    channel_to_publisher_count
        .into_iter()
        .filter(|(_, count)| *count == 0)
        .map(|(channel, _)| channel)
        .collect()
}

impl TaskGraphBuilder {
    pub fn new() -> TaskGraphBuilder {
        TaskGraphBuilder {
            pools: vec![],
            build_steps: vec![],
            execution_log_level_override: None,
            channel_registry: ChannelRegistry::new(),
            debug_info: false,
        }
    }

    /// Set the execution-log level for every callback node in the graph.
    /// Defaults to [`ExecutionLogLevel::Duration`]. Applied after build steps
    /// run, so build-step-created nodes are included. The graph level is
    /// applied to every node, overriding any per-node builder setting.
    pub fn with_execution_log_level(mut self, level: ExecutionLogLevel) -> TaskGraphBuilder {
        self.execution_log_level_override = Some(level);
        self
    }

    /// Set to `true` to compute dangling-channel diagnostics and return them on
    /// [`BuiltTaskGraph::debug_info`]. Defaults to `false`.
    pub fn with_debug_info(mut self, enabled: bool) -> TaskGraphBuilder {
        self.debug_info = enabled;
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

    /// Expose channel registry in case users need it. Most channels are
    /// registered automatically during [`build`](Self::build); this is an
    /// escape hatch for channels the graph can't introspect (e.g. forwarded
    /// channels, whose source-channel link is graph topology).
    pub fn channel_registry_mut(&mut self) -> &mut ChannelRegistry {
        &mut self.channel_registry
    }

    /// Register every node's channels into `registry`. Idempotent: `#[task_callback]`
    /// callbacks register their loggable ports via `Probe` (no-op for
    /// non-loggable types); hand-written callbacks default to a no-op.
    fn register_nodes(nodes: &[CallbackNode], registry: &mut ChannelRegistry) {
        for node in nodes {
            node.callback().register_channels(registry);
        }
    }

    pub fn build(mut self) -> Result<BuiltTaskGraph, TaskGraphBuildError> {
        // Move `self.pools` out before the loop so it can't be partially moved
        // (which would block borrowing `self.channel_registry` later).
        let pools_builders = self.pools;
        let pool_thread_counts: Vec<usize> =
            pools_builders.iter().map(|p| p.thread_count).collect();
        let mut all_nodes: Vec<CallbackNode> = Vec::new();
        let mut pool_node_counts: Vec<usize> = Vec::with_capacity(pools_builders.len());

        for mut pool in pools_builders {
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

        // Register the graph's channels before build steps run so they can
        // introspect the registry (e.g. the logging build step asks for a
        // serializer per publisher).
        Self::register_nodes(&all_nodes, &mut self.channel_registry);

        for step in self.build_steps.drain(..) {
            let step_name = step.name();
            let mut additional_nodes = step
                .build_step(&all_nodes, &mut self.channel_registry)
                .map_err(|error| TaskGraphBuildError::BuildStepError {
                    step_name: step_name.to_owned(),
                    error,
                })?;
            all_nodes.append(&mut additional_nodes);
        }

        // Build-step-created nodes may add channels of their own; register them
        // too (idempotent) so later steps and replay can resolve them.
        Self::register_nodes(&all_nodes, &mut self.channel_registry);

        if all_nodes.is_empty() {
            return Ok(BuiltTaskGraph {
                pools: vec![],
                execution_log_publishers: vec![],
                channel_registry: self.channel_registry,
                channel_interner: Default::default(),
                callback_interner: Default::default(),
                debug_info: None,
            });
        }

        let total_original: usize = pool_node_counts.iter().sum();
        let extra = all_nodes.len() - total_original;
        if pool_node_counts.is_empty() {
            if let Some(level_override) = self.execution_log_level_override {
                for node in all_nodes.iter_mut() {
                    node.set_execution_log_level(level_override);
                }
            }
            connect_callback_nodes(&mut all_nodes).map_err(TaskGraphBuildError::ConnectionError)?;
            let pools = vec![ThreadPoolConfig::new(1, all_nodes)];
            let debug_info = Self::compute_debug_info(self.debug_info, &pools);
            let (channel_interner, callback_interner) = Self::build_name_interners(&pools);
            return Ok(BuiltTaskGraph {
                pools,
                execution_log_publishers: vec![], // TODO: shouldn't this be populated with one publisher?
                channel_registry: self.channel_registry,
                channel_interner,
                callback_interner,
                debug_info,
            });
        }
        pool_node_counts[0] += extra;

        if let Some(level_override) = self.execution_log_level_override {
            for node in all_nodes.iter_mut() {
                node.set_execution_log_level(level_override);
            }
        }

        connect_callback_nodes(&mut all_nodes).map_err(TaskGraphBuildError::ConnectionError)?;

        let mut pools = Vec::with_capacity(pool_node_counts.len());
        for (i, &count) in pool_node_counts.iter().enumerate() {
            let nodes: Vec<CallbackNode> = all_nodes.drain(..count).collect();
            pools.push(ThreadPoolConfig::new(pool_thread_counts[i], nodes));
        }

        let mut execution_log_publishers = execution_log::log_publishers(&pools);
        execution_log::connect(&mut pools, &mut execution_log_publishers)
            .map_err(TaskGraphBuildError::ExecutionLogError)?;

        let debug_info = Self::compute_debug_info(self.debug_info, &pools);

        let (channel_interner, callback_interner) = Self::build_name_interners(&pools);
        Ok(BuiltTaskGraph {
            pools,
            execution_log_publishers,
            channel_registry: self.channel_registry,
            channel_interner,
            callback_interner,
            debug_info,
        })
    }

    /// Compute dangling-channel diagnostics when `with_debug_info(true)` was
    /// requested, otherwise `None`.
    fn compute_debug_info(debug_info: bool, pools: &[ThreadPoolConfig]) -> Option<GraphDebugInfo> {
        if !debug_info {
            return None;
        }
        Some(GraphDebugInfo {
            dangling_subscribers: find_dangling_subscribers(pools),
            dangling_publishers: find_dangling_publishers(pools),
        })
    }

    fn build_name_interners(
        pools: &[ThreadPoolConfig],
    ) -> (
        StringInterner<ChannelNameTag>,
        StringInterner<CallbackNameTag>,
    ) {
        let mut channel_interner = StringInterner::<ChannelNameTag>::default();
        let mut callback_interner = StringInterner::<CallbackNameTag>::default();
        for pool in pools.iter() {
            for shared_node in pool.nodes.iter_shared() {
                shared_node.access(|node| {
                    callback_interner.intern(node.name());
                    node.callback().for_each_subscriber(&mut |subscriber| {
                        channel_interner.intern(&subscriber.config().channel_name);
                    });
                    node.callback().for_each_publisher(&mut |publisher| {
                        channel_interner.intern(&publisher.config().channel_name);
                    });
                });
            }
        }
        (channel_interner, callback_interner)
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
        assert_eq!(
            built.pools[0].nodes[0].access(|n| n.name().to_owned()),
            "single"
        );
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
                _channel_registry: &mut ChannelRegistry,
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
        assert_eq!(
            built.pools[0].nodes[0].access(|n| n.name().to_owned()),
            "first"
        );
        assert_eq!(
            built.pools[0].nodes[1].access(|n| n.name().to_owned()),
            "extra_1"
        );
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
                _channel_registry: &mut ChannelRegistry,
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
    fn with_debug_info_returns_diagnostics() {
        let callback = make_callback_node("debug", 0, &[], 0, &[]);
        let result = TaskGraphBuilder::new()
            .add_pool(1, |p| p.add_callback(callback))
            .with_debug_info(true)
            .build();

        assert!(result.is_ok());
        let built = result.unwrap();
        assert_eq!(built.pools.len(), 1);
        assert_eq!(built.pools[0].nodes.len(), 1);
        assert_eq!(
            built.pools[0].nodes[0].access(|n| n.name().to_owned()),
            "debug"
        );
        assert!(built.debug_info.is_some());
    }

    #[test]
    fn without_debug_info_no_diagnostics() {
        let callback = make_callback_node("nodebug", 0, &[], 0, &[]);
        let built = TaskGraphBuilder::new()
            .add_pool(1, |p| p.add_callback(callback))
            .build()
            .expect("build");
        assert!(built.debug_info.is_none());
    }
}
