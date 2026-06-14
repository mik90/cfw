use std::error::Error;
use std::fmt;
use std::num::Saturating;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

use simulation_executor::SimulationConfig;
use simulation_executor::state::{SimulationState, StepError};
use task::callback::{ConnectedCallback, connect_callbacks};
use task::executor::ThreadPoolConfig;
use task::generic_publisher::GenericPublisher;
use task::pub_sub::{CallbackName, ChannelName};
use task::subscriber::GenericSubscriber;
use task::testing_publisher::TestPublisher;
use task::testing_subscriber::{DEFAULT_TEST_SUBSCRIBER_CAPACITY, TestSubscriber};
use task::time::FrameworkTime;

/// Struct for running unit tests against tasks
pub struct UnitTestExecutor {
    simulation_state: SimulationState,
    /// Mirrors the simulation's current time so `TestPublisher`s (which flush outside
    /// the normal step loop) can timestamp their messages with "now" rather than
    /// `start_time`. Updated at the end of every `try_step`.
    current_time: Arc<Mutex<FrameworkTime>>,
}

impl UnitTestExecutor {
    /// Create simple task tester
    pub fn new(tasks: Vec<ConnectedCallback>) -> Self {
        let pools = vec![ThreadPoolConfig {
            thread_count: 1,
            tasks,
        }];
        Self::new_with(UnitTestExecutorConfig {
            start_time: FrameworkTime::from_nanoseconds(0),
            pools,
            callback_executor_thread_count: 1,
        })
    }

    /// Create task tester with custom config
    pub fn new_with(config: UnitTestExecutorConfig) -> Self {
        Self::new_with_time_cell(config, None)
    }

    fn new_with_time_cell(
        config: UnitTestExecutorConfig,
        current_time: Option<Arc<Mutex<FrameworkTime>>>,
    ) -> Self {
        let start_time = config.start_time;
        let mut task_test = Self {
            simulation_state: SimulationState::new_with(SimulationConfig {
                start_time: config.start_time,
                pools: config.pools,
                callback_executor_thread_count: config.callback_executor_thread_count,
            }),
            current_time: current_time.unwrap_or_else(|| Arc::new(Mutex::new(start_time))),
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
        let before = self.simulation_state.get_simulation_time();
        self.simulation_state.step()?;
        let after = self.simulation_state.get_simulation_time();
        *self.current_time.lock().unwrap() = after;
        Ok(StepResult { before, after })
    }

    pub fn get_step_count(&self) -> Saturating<usize> {
        self.simulation_state.get_step_count()
    }

    pub fn get_current_time(&self) -> FrameworkTime {
        self.simulation_state.get_simulation_time()
    }
}

/// Builds a `UnitTestExecutor` while allowing `TestPublisher`/`TestSubscriber` fixtures
/// to be wired directly into a `ConnectedCallback`'s subscriber/publisher arenas.
///
/// This must happen *before* `connect_callbacks` runs: a freshly built `ConnectedCallback`
/// is unconnected (its arenas aren't allocated until `connect_callbacks` wires remaining
/// matches and sizes/allocates based on final capacities — see `connect_callbacks` in
/// `task::callback`). The builder owns the unconnected callbacks, lets the test attach
/// fixtures (which bump capacities as a side effect of connecting), and only then finalizes.
pub struct UnitTestExecutorBuilder {
    tasks: Vec<ConnectedCallback>,
    start_time: FrameworkTime,
    /// Shared with every `TestPublisher` created via this builder, and later handed to
    /// the resulting `UnitTestExecutor` so both stay in sync on "current sim time".
    current_time: Arc<Mutex<FrameworkTime>>,
}

impl UnitTestExecutorBuilder {
    pub fn new(tasks: Vec<ConnectedCallback>) -> Self {
        let start_time = FrameworkTime::from_nanoseconds(0);
        UnitTestExecutorBuilder {
            tasks,
            start_time,
            current_time: Arc::new(Mutex::new(start_time)),
        }
    }

    /// Find all publishers on the given channel
    fn find_publishers_mut(
        &mut self,
        channel_name: &str,
    ) -> Vec<(&mut (dyn GenericPublisher + 'static), CallbackName)> {
        self.tasks
            .iter_mut()
            .flat_map(|callback| {
                // Get all publishers matching the requested channel and the name of the task they're on
                let callback_name = callback.get_name().to_owned();
                let publisher_and_callback_name = callback
                    .get_publishers_mut()
                    .iter_mut()
                    // only take in publishers with the given channel name
                    .filter(|publisher| publisher.get_config().channel_name == *channel_name)
                    // Deref the box so callers don't need to care about it
                    .map(move |p| (p.deref_mut(), callback_name.clone()));

                publisher_and_callback_name
            })
            .collect()
    }

    /// Find all publishers on the given channel
    fn find_subscribers_mut(
        &mut self,
        channel_name: &str,
    ) -> Vec<(&mut (dyn GenericSubscriber + 'static), CallbackName)> {
        self.tasks
            .iter_mut()
            .flat_map(|callback| {
                // Get all subscribers matching the requested channel and the name of the task they're on
                let callback_name = callback.get_name().to_owned();
                let subscriber_and_callback_name = callback
                    .get_subscribers_mut()
                    .iter_mut()
                    // only take in subscribers with the given channel name
                    .filter(|subscriber| subscriber.get_config().channel_name == *channel_name)
                    // Deref the box so callers don't need to care about it
                    .map(move |p| (p.deref_mut(), callback_name.clone()));

                subscriber_and_callback_name
            })
            .collect()
    }

    /// Connects a `TestPublisher<T>` directly to the named subscriber on the callback at
    /// `task_index`, feeding it input in isolation. Since a test publisher feeds exactly
    /// one subscriber, its arena can be allocated immediately.
    pub fn add_test_publisher<T: Default + 'static>(
        &mut self,
        channel_name: &str,
    ) -> TestPublisher<T> {
        let current_time = self.current_time.clone();
        let time_source: Arc<Mutex<Box<dyn Fn() -> FrameworkTime>>> =
            Arc::new(Mutex::new(Box::new(move || *current_time.lock().unwrap())));

        let subscribers = self.find_subscribers_mut(channel_name);
        if subscribers.is_empty() {
            panic!("No subscriber for channel '{channel_name}'")
        }

        let capacity_of_all_subscribers = subscribers
            .iter()
            .map(|(subscriber, _)| subscriber.get_config().capacity)
            .sum();

        let mut publisher = TestPublisher::<T>::new(
            channel_name.to_string(),
            capacity_of_all_subscribers,
            time_source,
        );

        // find capacity for our publisher
        for (subscriber, callback_name) in subscribers {
            publisher
            .connect_to_subscriber(subscriber)
            .unwrap_or_else(|_| {
                panic!(
                    "Type mismatch connecting TestPublisher to channel '{channel_name}' on callback '{callback_name}'"
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

        for (publisher, callback_name) in publishers {
            publisher
                .connect_to_subscriber(&mut subscriber)
                .unwrap_or_else(|_| {
                    panic!("Type mismatch connecting TestSubscriber to channel '{channel_name}' on callback '{callback_name}'")
                });
        }

        subscriber
    }

    /// Wires up any remaining real connections (and allocates the callbacks' own publisher
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
        connect_callbacks(&mut self.tasks)?;

        let pools = vec![ThreadPoolConfig {
            thread_count: 1,
            tasks: self.tasks,
        }];
        let executor = UnitTestExecutor::new_with_time_cell(
            UnitTestExecutorConfig {
                start_time: self.start_time,
                pools,
                callback_executor_thread_count: 1,
            },
            Some(self.current_time),
        );
        Ok(executor)
    }
}

/// Configuration for running tasks
pub struct UnitTestExecutorConfig {
    pub start_time: FrameworkTime,
    pub pools: Vec<ThreadPoolConfig>,
    /// Number of real OS threads used to execute callbacks in parallel within a step.
    /// Independent of any virtual thread pool sizes.
    pub callback_executor_thread_count: usize,
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
        let (tasks_under_test, task_info) = build_fizz_buzz_tasks();
        let publisher_runtime =
            tasks_under_test[task_info.integer_publisher_index].get_execution_duration();

        let mut expected_time = FrameworkTime::from_nanoseconds(0);

        let mut executor = UnitTestExecutor::new(tasks_under_test);

        assert_eq!(
            task_info.get_stored_strings(),
            Vec::<String>::new(),
            "Should be empty on start"
        );

        assert_eq!(executor.get_current_time(), expected_time);
        let step_result = executor.step();
        assert_eq!(step_result.before, expected_time);

        // We expect just the publisher to run
        expected_time += publisher_runtime;
        assert_eq!(
            executor.get_current_time(),
            expected_time,
            "We expect just the publisher to have run"
        );
        assert_eq!(step_result.after, expected_time)
    }

    #[test]
    fn step_all_callbacks() {
        let (tasks_under_test, task_info) = build_fizz_buzz_tasks();
        let publisher_runtime =
            tasks_under_test[task_info.integer_publisher_index].get_execution_duration();
        let fizz_buzz_runtime =
            tasks_under_test[task_info.fizz_buzz_index].get_execution_duration();
        let string_store_runtime =
            tasks_under_test[task_info.string_store_index].get_execution_duration();

        let mut expected_time = FrameworkTime::from_nanoseconds(0);

        let mut executor = UnitTestExecutor::new(tasks_under_test);

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

        assert_eq!(task_info.get_stored_strings(), vec!["FizzBuzz"]);
    }

    #[test]
    fn test_individual_callback() {
        // test single callback using test_publisher and test_subscriber

        let calculator = FizzBuzzCalculator::build_connected_callback();

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
        expected = "Type mismatch connecting TestPublisher to channel 'integer' on callback 'FizzBuzzCalculator'"
    )]
    fn test_publisher_type_mismatch_fails() {
        let calculator = FizzBuzzCalculator::build_connected_callback();
        let mut builder = UnitTestExecutorBuilder::new(vec![calculator]);

        // Should panic since integer doesn't take a string
        let _ = builder.add_test_publisher::<String>("integer");
    }

    #[test]
    #[should_panic(
        expected = "Type mismatch connecting TestSubscriber to channel 'fizz_buzz_string' on callback 'FizzBuzzCalculator'"
    )]
    fn test_susbcriber_type_mismatch_fails() {
        let calculator = FizzBuzzCalculator::build_connected_callback();
        let mut builder = UnitTestExecutorBuilder::new(vec![calculator]);

        // Should panic since integer doesn't take a string
        let _ = builder.add_test_subscriber::<u8>("fizz_buzz_string");
    }
}
