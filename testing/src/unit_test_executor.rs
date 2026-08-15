use std::error::Error;
use std::fmt;
use std::num::Saturating;
use std::sync::Arc;

use simulation_executor::SimulationConfig;
use simulation_executor::state::{SimulationState, StepError};
use task::callback::{CallbackNode, CallbackViews, connect_callback_nodes};
use task::callback_storage::CallbackStorage;
use task::executor::ThreadPoolConfig;
use task::generic_publisher::GenericPublisher;
use task::pub_sub::CallbackNodeName;
use task::subscriber::GenericSubscriber;
use task::testing_publisher::TestPublisher;
use task::testing_subscriber::{DEFAULT_TEST_SUBSCRIBER_CAPACITY, TestSubscriber};
use task::testing_time::TimeSource;
use task::time::FrameworkTime;

/// Struct for running unit tests against callback nodes
pub struct UnitTestExecutor {
    simulation_state: SimulationState,
    /// Shared time cell updated at the end of every `try_step`. `TestPublisher`s
    /// created by the builder hold a clone so they timestamp messages with "now".
    time_source: Arc<TimeSource>,
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
        let mut task_test = Self {
            simulation_state: SimulationState::new_with(SimulationConfig {
                start_time: config.start_time,
                pools: config.pools,
                node_executor_thread_count: config.node_executor_thread_count,
            }),
            time_source: time_source.unwrap_or_else(|| Arc::new(TimeSource::new(start_time))),
        };
        task_test.simulation_state.start();
        task_test
    }

    /// Runs simulation, returning time before/after
    /// Panics on step failure
    pub fn step(&mut self) -> StepResult {
        self.try_step()
            .unwrap_or_else(|e| panic!("Could not step: {:?}", e))
    }

    /// Runs simulation, returning time before/after
    pub fn try_step(&mut self) -> Result<StepResult, StepError> {
        let before = self.simulation_state.simulation_time();
        self.simulation_state.step()?;
        let after = self.simulation_state.simulation_time();
        self.time_source.set(after);
        Ok(StepResult { before, after })
    }

    pub fn step_count(&self) -> Saturating<usize> {
        self.simulation_state.step_count()
    }

    pub fn current_time(&self) -> FrameworkTime {
        self.simulation_state.simulation_time()
    }
}

/// Builds a `UnitTestExecutor` while allowing `TestPublisher`/`TestSubscriber` fixtures
/// to be wired directly into a `CallbackNode`'s subscriber/publisher arenas.
///
/// This must happen *before* `connect_callback_nodes` runs: a freshly built `CallbackNode`
/// is unconnected (its arenas aren't allocated until `connect_callback_nodes` wires remaining
/// matches and sizes/allocates based on final capacities — see `connect_callback_nodes` in
/// `task::callback`). The builder owns the unconnected callback nodes, lets the test attach
/// fixtures (which bump capacities as a side effect of connecting), and only then finalizes.
pub struct UnitTestExecutorBuilder {
    nodes: Vec<CallbackNode>,
    start_time: FrameworkTime,
    /// Shared with every `TestPublisher` created via this builder, and later
    /// handed to the resulting `UnitTestExecutor` so both stay in sync.
    time_source: Arc<TimeSource>,
}

impl UnitTestExecutorBuilder {
    pub fn new(nodes: Vec<CallbackNode>) -> Self {
        let start_time = FrameworkTime::from_nanoseconds(0);
        UnitTestExecutorBuilder {
            nodes,
            start_time,
            time_source: Arc::new(TimeSource::new(start_time)),
        }
    }

    /// Find all publishers on the given channel
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

    /// Find all publishers on the given channel
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
    pub fn add_test_publisher<T: Default + 'static>(
        &mut self,
        channel_name: &str,
    ) -> TestPublisher<T> {
        let time_source = self.time_source.clone();

        let subscribers = self.find_subscribers_mut(channel_name);
        if subscribers.is_empty() {
            panic!("No subscriber for channel '{channel_name}'")
        }

        let capacity_of_all_subscribers = subscribers
            .iter()
            .map(|(subscriber, _)| subscriber.config().capacity)
            .sum();

        let mut publisher = TestPublisher::<T>::new(
            channel_name.to_string(),
            capacity_of_all_subscribers,
            time_source,
        );

        // find capacity for our publisher
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
        publisher
    }

    /// Connects a `TestSubscriber<T>` to `channel_name`, capturing its output in isolation, with the default queue depth
    /// ([`DEFAULT_TEST_SUBSCRIBER_CAPACITY`]). Use [`Self::add_test_subscriber_with_capacity`]
    /// if a test pushes through more messages than that comfortably holds.
    pub fn add_test_subscriber<T: 'static + Clone>(
        &mut self,
        channel_name: &str,
    ) -> TestSubscriber<T> {
        self.add_test_subscriber_with_capacity(channel_name, DEFAULT_TEST_SUBSCRIBER_CAPACITY)
    }

    /// Like [`Self::add_test_subscriber`], but with a caller-chosen queue depth.
    pub fn add_test_subscriber_with_capacity<T: 'static + Clone>(
        &mut self,
        channel_name: &str,
        capacity: usize,
    ) -> TestSubscriber<T> {
        let publishers = self.find_publishers_mut(channel_name);
        if publishers.is_empty() {
            panic!("No publisher for channel '{channel_name}'")
        }

        let mut subscriber = TestSubscriber::<T>::with_capacity(channel_name.to_string(), capacity);

        for (publisher, node_name) in publishers {
            publisher
                .connect_to_subscriber(&mut subscriber)
                .unwrap_or_else(|_| {
                    panic!("Type mismatch connecting TestSubscriber to channel '{channel_name}' on callback node '{node_name}'")
                });
        }

        subscriber
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
    pub fn try_build(mut self) -> Result<UnitTestExecutor, Box<dyn Error>> {
        connect_callback_nodes(&mut self.nodes)?;

        let pools = vec![ThreadPoolConfig::new(1, self.nodes)];
        let executor = UnitTestExecutor::new_with_time_source(
            UnitTestExecutorConfig {
                start_time: self.start_time,
                pools,
                node_executor_thread_count: 1,
            },
            Some(self.time_source),
        );
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

    #[test]
    fn step_time_before_after() {
        let (nodes_under_test, task_info) = build_fizz_buzz_callback_nodes();
        let publisher_runtime = nodes_under_test[task_info.integer_publisher_index]
            .with_exclusive(|n| n.execution_duration());

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
        let publisher_runtime = nodes_under_test[task_info.integer_publisher_index]
            .with_exclusive(|n| n.execution_duration());
        let fizz_buzz_runtime =
            nodes_under_test[task_info.fizz_buzz_index].with_exclusive(|n| n.execution_duration());
        let string_store_runtime = nodes_under_test[task_info.string_store_index]
            .with_exclusive(|n| n.execution_duration());

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

        let messages = string_subscriber.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message, "FizzBuzz");
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
