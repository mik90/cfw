use std::error::Error;
use std::fmt;
use std::num::Saturating;
use std::sync::{Arc, Mutex};

use simulation_executor::SimulationConfig;
use simulation_executor::state::{SimulationState, StepError};
use task::callback::{CallbackNode, CallbackViews};
use task::callback_storage::CallbackStorage;
use task::executor::{ExecutorParams, ThreadPoolConfig};
use task::pub_sub::CallbackNodeName;
use task::task_graph_builder::TaskGraphBuilder;
use task::testing_publisher::TestPublisher;
use task::testing_subscriber::{DEFAULT_TEST_SUBSCRIBER_CAPACITY, TestSubscriber};
use task::testing_time::TimeSource;
use task::time::FrameworkTime;
use task::{
    GenericPublisher, GenericSubscriber, Publisher, PublisherConfig, Subscriber, SubscriberConfig,
};

#[cfg(feature = "iceoryx2")]
mod iox2;
#[cfg(feature = "iceoryx2")]
pub use iox2::{Iox2TestNotifier, Iox2TestPublisher, Iox2TestSubscriber};

/// Internal storage for all graph components
struct UnitTestGraph {
    simulation_state: SimulationState,

    execution_log_publishers:
        Vec<task::publisher::Publisher<task::execution_log::ExecutionLogMessage>>,

    /// Storage of test publishers and subscribers.
    test_publishers: Vec<Arc<Mutex<dyn task::generic_publisher::GenericPublisher>>>,
    test_subscribers: Vec<Arc<Mutex<dyn task::generic_subscriber::GenericSubscriber>>>,
}

/// Struct for running unit tests against callback nodes.
/// Dropping the executor clears queued native fixture messages before releasing their arenas.
pub struct UnitTestExecutor {
    unit_test_graph: UnitTestGraph,
    /// Shared time cell updated at the end of every `try_step`. `TestPublisher`s
    /// created by the builder hold a clone so they timestamp messages with "now".
    time_source: Arc<TimeSource>,
    #[cfg(feature = "iceoryx2")]
    iox2_fixtures: Vec<Box<dyn iox2::Iox2FixturePort>>,
    #[cfg(feature = "iceoryx2")]
    iox2_activation: iox2::ActivationCell,
}

impl UnitTestExecutor {
    /// Create simple callback node tester
    pub fn new(nodes: impl Into<CallbackStorage>) -> Self {
        let pools = vec![ThreadPoolConfig::new(1, nodes)];
        Self::new_with(UnitTestExecutorConfig {
            start_time: FrameworkTime::from_nanoseconds(0),
            pools,
            node_executor_thread_count: 1,
        })
    }

    /// Create callback node tester with custom config
    pub fn new_with(config: UnitTestExecutorConfig) -> Self {
        Self::new_with_time_source(config, None)
    }

    fn new_with_time_source(
        config: UnitTestExecutorConfig,
        time_source: Option<Arc<TimeSource>>,
    ) -> Self {
        let start_time = config.start_time;
        #[cfg(feature = "iceoryx2")]
        let iox2_activation = iox2::inactive_cell();

        let simulation_state = SimulationState::new_with(SimulationConfig {
            start_time: config.start_time,
            executor_params: ExecutorParams::new(config.pools),
            node_executor_thread_count: config.node_executor_thread_count,
        });

        let unit_test_graph = UnitTestGraph {
            simulation_state,
            test_publishers: Vec::new(),
            test_subscribers: Vec::new(),
            execution_log_publishers: Vec::new(),
        };
        let mut unit_test_executor = Self {
            unit_test_graph,
            time_source: time_source.unwrap_or_else(|| Arc::new(TimeSource::new(start_time))),
            #[cfg(feature = "iceoryx2")]
            iox2_fixtures: Vec::new(),
            #[cfg(feature = "iceoryx2")]
            iox2_activation,
        };
        unit_test_executor.unit_test_graph.simulation_state.start();
        #[cfg(feature = "iceoryx2")]
        iox2::activate(&unit_test_executor.iox2_activation);
        unit_test_executor
    }

    /// Runs simulation, returning time before/after
    /// Panics on step failure
    pub fn step(&mut self) -> StepResult {
        self.try_step()
            .unwrap_or_else(|e| panic!("Could not step: {:?}", e))
    }

    /// Runs simulation, returning time before/after
    pub fn try_step(&mut self) -> Result<StepResult, StepError> {
        let simulation_state = &mut self.unit_test_graph.simulation_state;
        let before = simulation_state.simulation_time();
        simulation_state.step()?;
        let after = simulation_state.simulation_time();
        self.time_source.set(after);
        Ok(StepResult { before, after })
    }

    pub fn step_count(&self) -> Saturating<usize> {
        self.unit_test_graph.simulation_state.step_count()
    }

    pub fn current_time(&self) -> FrameworkTime {
        self.unit_test_graph.simulation_state.simulation_time()
    }
}

impl Drop for UnitTestExecutor {
    fn drop(&mut self) {
        let _ = self
            .unit_test_graph
            .simulation_state
            .shutdown_node_executor_threads();

        #[cfg(feature = "iceoryx2")]
        {
            iox2::closed(&self.iox2_activation);
            for fixture in &self.iox2_fixtures {
                fixture.close();
            }
        }

        // Fixture handles can retain subscribers after the executor's Arc is released.
        for subscriber in &self.unit_test_graph.test_subscribers {
            subscriber
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .cleanup_buffers();
        }
        self.unit_test_graph.test_subscribers.clear();
        self.unit_test_graph.simulation_state.clear_subscribers();
        self.unit_test_graph.test_publishers.clear();
    }
}

/// Builds a `UnitTestExecutor` with native fixtures and iox2 output captures.
///
/// This must happen *before* `connect_callback_nodes` runs: a freshly built `CallbackNode`
/// is unconnected (its arenas aren't allocated until `connect_callback_nodes` wires remaining
/// matches and sizes/allocates based on final capacities — see `connect_callback_nodes` in
/// `task::callback`). The builder owns the unconnected callback nodes, lets the test attach
/// fixtures (which bump capacities as a side effect of connecting), and only then finalizes.
/// Iox2 input and event handles become usable after [`Self::build`] opens the graph services.
pub struct UnitTestExecutorBuilder {
    nodes: Vec<CallbackNode>,
    test_publishers: Vec<Arc<Mutex<dyn GenericPublisher>>>,
    test_subscribers: Vec<Arc<Mutex<dyn GenericSubscriber>>>,

    start_time: FrameworkTime,
    /// Shared with every `TestPublisher` created via this builder, and later
    /// handed to the resulting `UnitTestExecutor` so both stay in sync.
    time_source: Arc<TimeSource>,
    #[cfg(feature = "iceoryx2")]
    iox2_fixtures: Vec<Box<dyn iox2::Iox2FixturePort>>,
    #[cfg(feature = "iceoryx2")]
    iox2_activation: iox2::ActivationCell,
}

impl UnitTestExecutorBuilder {
    pub fn new(nodes: Vec<CallbackNode>) -> Self {
        let start_time = FrameworkTime::from_nanoseconds(0);
        UnitTestExecutorBuilder {
            nodes,
            test_publishers: Vec::new(),
            test_subscribers: Vec::new(),
            start_time,
            time_source: Arc::new(TimeSource::new(start_time)),
            #[cfg(feature = "iceoryx2")]
            iox2_fixtures: Vec::new(),
            #[cfg(feature = "iceoryx2")]
            iox2_activation: iox2::inactive_cell(),
        }
    }

    pub fn add_node(mut self, node: CallbackNode) -> Self {
        self.nodes.push(node);
        self
    }

    /// Find all publishers on the given channel for the system under test
    fn find_publishers_mut(
        &mut self,
        channel_name: &str,
    ) -> Vec<(&mut dyn GenericPublisher, CallbackNodeName)> {
        self.nodes
            .iter_mut()
            .flat_map(|node| {
                // Get all publishers matching the requested channel and the name of the node they're on
                let node_name = node.name().to_owned();

                node.callback_mut()
                    .collect_publishers_mut()
                    .into_iter()
                    // only take in publishers with the given channel name
                    .filter(|publisher| publisher.config().channel_name == *channel_name)
                    .map(move |p| (p, node_name.clone()))
            })
            .collect()
    }

    /// Find all publishers on the given channel for the system under test
    fn find_subscribers_mut(
        &mut self,
        channel_name: &str,
    ) -> Vec<(&mut dyn GenericSubscriber, CallbackNodeName)> {
        self.nodes
            .iter_mut()
            .flat_map(|node| {
                // Get all subscribers matching the requested channel and the name of the node they're on
                let node_name = node.name().to_owned();

                node.callback_mut()
                    .collect_subscribers_mut()
                    .into_iter()
                    // only take in subscribers with the given channel name
                    .filter(|subscriber| subscriber.config().channel_name == *channel_name)
                    .map(move |p| (p, node_name.clone()))
            })
            .collect()
    }

    /// Connects a `TestPublisher<T>` directly to the named subscriber on the callback node at
    /// `node_index`, feeding it input in isolation. Since a test publisher feeds exactly
    /// one subscriber, its arena can be allocated immediately.
    // The fixture moves values/final drops between workers (`Send`), supports
    // shared reads (`Sync`), and may retain queued values (`'static`).
    pub fn add_test_publisher<T: Default + Send + Sync + 'static>(
        &mut self,
        channel_name: &str,
    ) -> TestPublisher<T> {
        let time_source = self.time_source.clone();

        let subscribers = self.find_subscribers_mut(channel_name);
        if subscribers.is_empty() {
            panic!("No subscriber for channel '{channel_name}'")
        }

        // find capacity for our publisher
        let capacity_of_all_subscribers = subscribers
            .iter()
            .map(|(subscriber, _)| subscriber.config().capacity)
            .sum();

        let mut publisher = Publisher::<T>::new(PublisherConfig {
            capacity: capacity_of_all_subscribers,
            channel_name: channel_name.to_string(),
        });

        for (subscriber, node_name) in subscribers {
            publisher
            .connect_to_subscriber(subscriber)
            .unwrap_or_else(|_| {
                panic!(
                    "Type mismatch connecting TestPublisher to channel '{channel_name}' on callback node '{node_name}'"
                )
            });
        }
        publisher.allocate_arena();

        let publisher_arc = Arc::new(Mutex::new(publisher));
        self.test_publishers.push(publisher_arc.clone());
        TestPublisher::new(publisher_arc, time_source)
    }

    /// Connects a `TestSubscriber<T>` to `channel_name`, capturing its output in isolation, with the default queue depth
    /// ([`DEFAULT_TEST_SUBSCRIBER_CAPACITY`]). Use [`Self::add_test_subscriber_with_capacity`]
    /// if a test pushes through more messages than that comfortably holds.
    // The fixture moves values/final drops between workers (`Send`), supports
    // shared reads (`Sync`), and may retain queued values (`'static`).
    pub fn add_test_subscriber<T: Send + Sync + 'static>(
        &mut self,
        channel_name: &str,
    ) -> TestSubscriber<T> {
        self.add_test_subscriber_with_capacity(channel_name, DEFAULT_TEST_SUBSCRIBER_CAPACITY)
    }

    /// Like [`Self::add_test_subscriber`], but with a caller-chosen queue depth.
    // The fixture has the same `Send`, `Sync`, and `'static` channel boundary
    // described by `add_test_subscriber` above.
    pub fn add_test_subscriber_with_capacity<T: Send + Sync + 'static>(
        &mut self,
        channel_name: &str,
        capacity: usize,
    ) -> TestSubscriber<T> {
        let publishers = self.find_publishers_mut(channel_name);
        if publishers.is_empty() {
            panic!("No publisher for channel '{channel_name}'")
        }

        let mut subscriber = Subscriber::<T>::new(SubscriberConfig {
            channel_name: channel_name.to_string(),
            capacity,
            is_optional: true,
            is_trigger: false,
            keep_across_runs: true,
        });

        for (publisher, node_name) in publishers {
            publisher
                .connect_to_subscriber(&mut subscriber)
                .unwrap_or_else(|_| {
                    panic!("Type mismatch connecting TestSubscriber to channel '{channel_name}' on callback node '{node_name}'")
                });
        }

        let subscriber_arc = Arc::new(Mutex::new(subscriber));
        self.test_subscribers.push(subscriber_arc.clone());
        TestSubscriber::new(subscriber_arc)
    }

    /// Wires up any remaining real connections (and allocates the callback nodes' own publisher
    /// arenas, now correctly sized — test connections above already bumped capacities where
    /// needed), then constructs the executor.
    pub fn build(self) -> UnitTestExecutor {
        match self.try_build() {
            Ok(e) => e,
            Err(e) => {
                panic!("Could not build unit test executor: {e}");
            }
        }
    }

    /// Same as build(), but exposes error cases.
    pub fn try_build(self) -> Result<UnitTestExecutor, Box<dyn Error>> {
        #[cfg(feature = "iceoryx2")]
        let fixture_endpoints = self
            .iox2_fixtures
            .iter()
            .map(|fixture| fixture.endpoint())
            .collect::<Vec<_>>();
        let graph_builder = TaskGraphBuilder::new().add_pool(1, |pool| {
            self.nodes
                .into_iter()
                .fold(pool, |p, node| p.add_callback(node))
        });
        #[cfg(feature = "iceoryx2")]
        let graph_builder = graph_builder.with_iox2_extra_endpoints(fixture_endpoints);
        let mut graph = graph_builder
            .build()
            .map_err(|error| -> Box<dyn Error> { error.to_string().into() })?;
        let pools = std::mem::take(&mut graph.pools);
        let execution_log_publishers = std::mem::take(&mut graph.execution_log_publishers);
        let params = ExecutorParams::new(pools);
        #[cfg(feature = "iceoryx2")]
        if let Some(context) = graph.iox2_context.as_mut() {
            for fixture in &self.iox2_fixtures {
                if let Err(error) = fixture.open(context) {
                    for fixture in &self.iox2_fixtures {
                        fixture.close();
                    }
                    return Err(error.to_string().into());
                }
            }
        }
        #[cfg(feature = "iceoryx2")]
        let params = params.with_iox2_context(graph.iox2_context.take());
        let start_time = self.start_time;
        let state = SimulationState::try_new_with(SimulationConfig {
            start_time,
            executor_params: params,
            node_executor_thread_count: 1,
        });
        #[cfg(feature = "iceoryx2")]
        if state.is_err() {
            for fixture in &self.iox2_fixtures {
                fixture.close();
            }
        }
        let mut simulation_state =
            state.map_err(|error| -> Box<dyn Error> { format!("{error:?}").into() })?;
        simulation_state.start();
        #[cfg(feature = "iceoryx2")]
        iox2::activate(&self.iox2_activation);

        let unit_test_graph = UnitTestGraph {
            simulation_state,
            test_publishers: self.test_publishers,
            test_subscribers: self.test_subscribers,
            execution_log_publishers,
        };
        let executor = UnitTestExecutor {
            unit_test_graph,
            time_source: self.time_source,
            #[cfg(feature = "iceoryx2")]
            iox2_fixtures: self.iox2_fixtures,
            #[cfg(feature = "iceoryx2")]
            iox2_activation: self.iox2_activation,
        };
        Ok(executor)
    }
}

/// Configuration for running callback nodes
pub struct UnitTestExecutorConfig {
    pub start_time: FrameworkTime,
    pub pools: Vec<ThreadPoolConfig>,
    /// Number of real OS threads used to execute callback nodes in parallel within a step.
    /// Independent of any virtual thread pool sizes.
    pub node_executor_thread_count: usize,
}

pub struct StepResult {
    before: FrameworkTime,
    after: FrameworkTime,
}

impl fmt::Display for StepResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Stepped from {} to {}", self.before, self.after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_tasks::*;

    #[cfg(feature = "iceoryx2")]
    #[test]
    #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
    fn captures_iox2_output_and_header_with_fixture_capacity() {
        use std::time::Duration;
        use task::callback::{Callback, PubOrSub, PubOrSubMut};
        use task::context::Context;
        use task::iox2::{Iox2Output, Iox2Publisher};
        use task::publisher::PublisherConfig;

        struct Source {
            output: Iox2Publisher<u64>,
            next: u64,
        }
        impl Callback for Source {
            fn run(&mut self, _ctx: &Context) {
                let mut output = Iox2Output::new_default(&mut self.output);
                *output = self.next;
                self.next += 1;
                output.send();
            }
            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Publisher(&self.output));
            }
            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Publisher(&mut self.output));
            }
        }

        let channel = format!("unit_test_capture_{}", std::process::id());
        let node = task::callback_builder::CallbackBuilder::new(
            "capture_source".into(),
            Box::new(Source {
                output: Iox2Publisher::new(PublisherConfig {
                    capacity: 1,
                    channel_name: channel.clone(),
                }),
                next: 42,
            }),
        )
        .with_execution_duration_callback(|| Duration::ZERO)
        .with_next_execution_time_callback(|now| Some(now + Duration::from_nanos(100)))
        .build()
        .unwrap();
        let mut builder = UnitTestExecutorBuilder::new(vec![node]);
        let capture = builder.add_iox2_test_subscriber_with_capacity::<u64>(&channel, 2);
        let mut executor = builder.build();
        for _ in 0..3 {
            executor.step();
        }
        let messages = capture.messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message, 43);
        assert_eq!(messages[1].message, 44);
        assert_eq!(
            messages[0].header.published_at,
            FrameworkTime::from_nanoseconds(100)
        );
        assert_eq!(
            messages[1].header.published_at,
            FrameworkTime::from_nanoseconds(200)
        );
        drop(executor);
        assert!(capture.try_messages().is_err());
    }

    #[cfg(feature = "iceoryx2")]
    #[test]
    #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
    fn iox2_data_and_counted_event_fixtures_drive_callback() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use task::callback::{Callback, PubOrSub, PubOrSubMut};
        use task::context::Context;
        use task::iox2::{Iox2Event, Iox2EventSubscriber, Iox2OptionalInput, Iox2Subscriber};
        use task::subscriber::{Subscriber, SubscriberConfig};

        type ObservedInput = (
            FrameworkTime,
            u64,
            Vec<(iceoryx2::prelude::EventId, u64)>,
            u64,
        );
        type ObservedInputs = Arc<Mutex<Vec<ObservedInput>>>;

        struct Inputs {
            data: Iox2Subscriber<u64>,
            event: Iox2EventSubscriber,
            gate: Subscriber<u64>,
            observed: ObservedInputs,
        }
        impl Callback for Inputs {
            fn run(&mut self, _ctx: &Context) {
                let data = Iox2OptionalInput::new(&self.data);
                let events = Iox2Event::new(&self.event);
                let gate = self.gate.read_buffer();
                if let (Some(header), Some(value), Some(required)) =
                    (data.header(), data.value(), gate.front())
                {
                    self.observed.lock().unwrap().push((
                        header.published_at,
                        *value,
                        events.records().collect(),
                        required.message,
                    ));
                }
            }
            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Subscriber(&self.data));
                f(PubOrSub::Subscriber(&self.event));
                f(PubOrSub::Subscriber(&self.gate));
            }
            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Subscriber(&mut self.data));
                f(PubOrSubMut::Subscriber(&mut self.event));
                f(PubOrSubMut::Subscriber(&mut self.gate));
            }
        }
        let channel = format!("unit_iox2_input_{}", std::process::id());
        let event_channel = format!("unit_iox2_event_{}", std::process::id());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let gate = Subscriber::new(SubscriberConfig {
            is_optional: false,
            capacity: 1,
            is_trigger: false,
            keep_across_runs: true,
            channel_name: "fixture_gate".into(),
        });
        let callback = Inputs {
            data: Iox2Subscriber::new(SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: false,
                keep_across_runs: true,
                channel_name: channel.clone(),
            }),
            event: Iox2EventSubscriber::new(SubscriberConfig {
                is_optional: true,
                capacity: 4,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: event_channel.clone(),
            }),
            gate,
            observed: Arc::clone(&observed),
        };
        let mut node = CallbackNode::new_named(Box::new(callback), "iox2_fixture_input".into());
        node.set_execution_duration_callback(Box::new(|| Duration::from_nanos(10)));
        let mut builder = UnitTestExecutorBuilder::new(vec![node]);
        let input = builder.add_iox2_test_publisher::<u64>(&channel);
        let events = builder.add_iox2_test_notifier(&event_channel);
        let mut gate_publisher = builder.add_test_publisher::<u64>("fixture_gate");
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| input.send(0))).is_err());
        let mut executor = builder.build();
        input.send(77);
        events.notify(iceoryx2::prelude::EventId::new(9), 5);
        executor.step();
        assert!(observed.lock().unwrap().is_empty());
        gate_publisher.send(11);
        executor.step();
        {
            let observed = observed.lock().unwrap();
            assert_eq!(observed.len(), 1);
            assert_eq!(observed[0].0, FrameworkTime::from_nanoseconds(0));
            assert_eq!(observed[0].1, 77);
            assert_eq!(observed[0].2, vec![(iceoryx2::prelude::EventId::new(9), 5)]);
            assert_eq!(observed[0].3, 11);
        }

        input.send(78);
        events.notify(iceoryx2::prelude::EventId::new(4), 2);
        executor.step();
        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[1].0, FrameworkTime::from_nanoseconds(10));
        assert_eq!(observed[1].1, 78);
        assert_eq!(observed[1].2, vec![(iceoryx2::prelude::EventId::new(4), 2)]);
        assert_eq!(observed[1].3, 11);
    }

    #[cfg(feature = "iceoryx2")]
    #[test]
    #[should_panic(
        expected = "Type mismatch connecting iox2 test publisher to channel 'macro_sensor'"
    )]
    fn iox2_publisher_fixture_rejects_wrong_payload_type() {
        let node = test_tasks::Iox2MacroDemo::build_callback_builder()
            .build()
            .unwrap();
        let mut builder = UnitTestExecutorBuilder::new(vec![node]);
        let _ = builder.add_iox2_test_publisher::<u32>("macro_sensor");
    }

    #[cfg(feature = "iceoryx2")]
    #[test]
    #[should_panic(expected = "iox2 test fixture cannot send until build() completes")]
    fn iox2_publisher_fixture_rejects_send_before_build() {
        let node = test_tasks::Iox2MacroDemo::build_callback_builder()
            .build()
            .unwrap();
        let mut builder = UnitTestExecutorBuilder::new(vec![node]);
        let input = builder.add_iox2_test_publisher::<u64>("macro_sensor");
        input.send(1);
    }

    #[cfg(feature = "iceoryx2")]
    #[test]
    #[should_panic(expected = "iox2 test fixture cannot send until build() completes")]
    fn iox2_notifier_fixture_rejects_notify_before_build() {
        let node = test_tasks::Iox2MacroDemo::build_callback_builder()
            .build()
            .unwrap();
        let mut builder = UnitTestExecutorBuilder::new(vec![node]);
        let notifier = builder.add_iox2_test_notifier("macro_events");
        notifier.notify(iceoryx2::prelude::EventId::new(0), 1);
    }

    #[cfg(feature = "iceoryx2")]
    #[test]
    #[should_panic(expected = "iox2 capture subscriber is not open until build() completes")]
    fn iox2_capture_fixture_rejects_read_before_build() {
        let node = test_tasks::Iox2MacroDemo::build_callback_builder()
            .build()
            .unwrap();
        let mut builder = UnitTestExecutorBuilder::new(vec![node]);
        let capture = builder.add_iox2_test_subscriber::<u64>("macro_output");
        capture.messages();
    }

    #[cfg(feature = "iceoryx2")]
    #[test]
    fn iox2_capture_rejects_native_publisher_on_same_channel() {
        let node = IncrementingIntegerPublisher::build_callback_node();
        let mut builder = UnitTestExecutorBuilder::new(vec![node]);
        let _capture = builder.add_iox2_test_subscriber::<u64>("integer");
        let error = builder
            .try_build()
            .err()
            .expect("mixed transports must fail");
        assert!(
            error.to_string().contains("mixes transports"),
            "unexpected error: {error}"
        );
    }

    #[cfg(feature = "iceoryx2")]
    #[test]
    #[should_panic(expected = "No iox2 event subscriber for channel 'missing_event'")]
    fn iox2_notifier_fixture_rejects_unknown_channel() {
        let mut builder = UnitTestExecutorBuilder::new(vec![]);
        let _ = builder.add_iox2_test_notifier("missing_event");
    }

    #[test]
    fn step_time_before_after() {
        let (nodes_under_test, task_info) = build_fizz_buzz_callback_nodes();
        let publisher_runtime =
            nodes_under_test[task_info.integer_publisher_index].access(|n| n.execution_duration());

        let mut expected_time = FrameworkTime::from_nanoseconds(0);

        let mut executor = UnitTestExecutor::new(nodes_under_test);

        assert_eq!(
            task_info.stored_strings(),
            Vec::<String>::new(),
            "Should be empty on start"
        );

        assert_eq!(executor.current_time(), expected_time);
        let step_result = executor.step();
        assert_eq!(step_result.before, expected_time);

        // We expect just the publisher to run
        expected_time += publisher_runtime;
        assert_eq!(
            executor.current_time(),
            expected_time,
            "We expect just the publisher to have run"
        );
        assert_eq!(step_result.after, expected_time)
    }

    #[test]
    fn step_all_callbacks() {
        let (nodes_under_test, task_info) = build_fizz_buzz_callback_nodes();
        let publisher_runtime =
            nodes_under_test[task_info.integer_publisher_index].access(|n| n.execution_duration());
        let fizz_buzz_runtime =
            nodes_under_test[task_info.fizz_buzz_index].access(|n| n.execution_duration());
        let string_store_runtime =
            nodes_under_test[task_info.string_store_index].access(|n| n.execution_duration());

        let mut expected_time = FrameworkTime::from_nanoseconds(0);

        let mut executor = UnitTestExecutor::new(nodes_under_test);

        let mut step_result = executor.step();

        assert_eq!(step_result.before, expected_time);
        // We expect just the publisher to run
        expected_time += publisher_runtime;
        assert_eq!(step_result.after, expected_time);

        step_result = executor.step();
        // We expect just fizz_buzz to run, since it was the only thing with input
        expected_time += fizz_buzz_runtime;
        assert_eq!(step_result.after, expected_time);

        step_result = executor.step();
        // We expect just the string store to run, since it was the only thing with input
        expected_time += string_store_runtime;
        assert_eq!(step_result.after, expected_time);

        assert_eq!(task_info.stored_strings(), vec!["FizzBuzz"]);
    }

    #[test]
    fn test_individual_callback() {
        // test single callback node using test_publisher and test_subscriber

        let calculator = FizzBuzzCalculator::build_callback_node();

        let mut builder = UnitTestExecutorBuilder::new(vec![calculator]);
        let mut integer_publisher = builder.add_test_publisher::<u64>("integer");
        let mut string_subscriber = builder.add_test_subscriber::<String>("fizz_buzz_string");
        let mut executor = builder.build();

        integer_publisher.send(15);
        executor.step();

        let mut messages = Vec::new();
        let count = string_subscriber.messages(|index, message| {
            assert_eq!(index, messages.len());
            messages.push(message.message.clone());
        });
        assert_eq!(count, 1);
        assert_eq!(messages, ["FizzBuzz"]);
    }

    #[test]
    fn native_messages_are_indexed_per_batch() {
        let mut source = IncrementingIntegerPublisher::build_callback_node();
        source.set_execution_duration_callback(Box::new(|| std::time::Duration::ZERO));
        let mut builder = UnitTestExecutorBuilder::new(vec![source]);
        let mut capture = builder.add_test_subscriber::<u64>("integer");
        let mut executor = builder.build();
        executor.step();
        executor.step();

        let count = capture.messages(|index, message| {
            assert_eq!(message.message, index as u64);
        });
        assert_eq!(count, 2);

        executor.step();
        assert_eq!(
            capture.messages(|index, message| {
                assert_eq!(index, 0);
                assert_eq!(message.message, 2);
            }),
            1
        );
        assert_eq!(capture.messages(|_, _| panic!("Batch should be empty")), 0);
    }

    #[test]
    fn native_try_messages_reports_overflow() {
        let mut source = IncrementingIntegerPublisher::build_callback_node();
        source.set_execution_duration_callback(Box::new(|| std::time::Duration::ZERO));
        let mut builder = UnitTestExecutorBuilder::new(vec![source]);
        let mut capture = builder.add_test_subscriber_with_capacity::<u64>("integer", 1);
        let mut executor = builder.build();
        executor.step();
        executor.step();

        let (count, dropped) = capture.try_messages(|index, message| {
            assert_eq!(index, 0);
            assert_eq!(message.message, 1);
        });
        assert_eq!(count, 1);
        assert_eq!(dropped.writer, 1);
        assert_eq!(dropped.reader, 0);
    }

    #[test]
    #[should_panic(
        expected = "Type mismatch connecting TestPublisher to channel 'integer' on callback node 'FizzBuzzCalculator'"
    )]
    fn test_publisher_type_mismatch_fails() {
        let calculator = FizzBuzzCalculator::build_callback_node();
        let mut builder = UnitTestExecutorBuilder::new(vec![calculator]);

        // Should panic since integer doesn't take a string
        let _ = builder.add_test_publisher::<String>("integer");
    }

    #[test]
    #[should_panic(
        expected = "Type mismatch connecting TestSubscriber to channel 'fizz_buzz_string' on callback node 'FizzBuzzCalculator'"
    )]
    fn test_susbcriber_type_mismatch_fails() {
        let calculator = FizzBuzzCalculator::build_callback_node();
        let mut builder = UnitTestExecutorBuilder::new(vec![calculator]);

        // Should panic since integer doesn't take a string
        let _ = builder.add_test_subscriber::<u8>("fizz_buzz_string");
    }
}
