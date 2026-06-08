use std::fmt;
use std::num::Saturating;
use std::sync::{Arc, Mutex};

use simulation_executor::SimulationConfig;
use simulation_executor::state::{SimulationState, StepError};
use task::callback::{ConnectedCallback, connect_callbacks};
use task::executor::ThreadPoolConfig;
use task::generic_publisher::GenericPublisher;
use task::testing_publisher::TestPublisher;
use task::testing_subscriber::TestSubscriber;
use task::time::FrameworkTime;

/// Capacity used for `TestSubscriber`s built by `UnitTestExecutorBuilder`. Small and
/// finite — the underlying queue (and the arena slots it requires, see
/// `Publisher::add_foreign_subscriber`) must be bounded, and unit tests only ever
/// push through a handful of messages at a time.
const TEST_SUBSCRIBER_CAPACITY: usize = 64;

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

    /// Connects a `TestPublisher<T>` directly to the named subscriber on the callback at
    /// `task_index`, feeding it input in isolation. Since a test publisher feeds exactly
    /// one subscriber, its arena can be allocated immediately.
    pub fn add_test_publisher<T: Default + 'static>(
        &mut self,
        task_index: usize,
        channel_name: &str,
    ) -> TestPublisher<T> {
        let current_time = self.current_time.clone();
        let time_source: Arc<Mutex<Box<dyn Fn() -> FrameworkTime>>> =
            Arc::new(Mutex::new(Box::new(move || *current_time.lock().unwrap())));

        let subscriber = self.tasks[task_index]
            .find_subscriber_mut(channel_name)
            .unwrap_or_else(|| {
                panic!("No subscriber for channel '{channel_name}' on task {task_index}")
            });
        let capacity = subscriber.get_config().capacity;

        let mut publisher = TestPublisher::<T>::new(channel_name.to_string(), capacity, time_source);
        publisher
            .connect_to_subscriber(subscriber.as_mut())
            .unwrap_or_else(|_| {
                panic!("Type mismatch connecting TestPublisher to channel '{channel_name}'")
            });
        publisher.allocate_arena();
        publisher
    }

    /// Connects a `TestSubscriber<T>` directly to the named publisher on the callback at
    /// `task_index`, capturing its output in isolation. Connects as a "foreign" (zero-copy,
    /// non-arena) subscriber — see `Publisher::add_foreign_subscriber` — which bumps the
    /// publisher's arena size by `TEST_SUBSCRIBER_CAPACITY`; `connect_callbacks`'s later
    /// `allocate_arena` picks up that bump along with any other connections.
    pub fn add_test_subscriber<T: 'static + Clone>(
        &mut self,
        task_index: usize,
        channel_name: &str,
    ) -> TestSubscriber<T> {
        let publisher = self.tasks[task_index]
            .find_publisher_mut(channel_name)
            .unwrap_or_else(|| {
                panic!("No publisher for channel '{channel_name}' on task {task_index}")
            });

        let mut subscriber = TestSubscriber::<T>::new(channel_name.to_string(), TEST_SUBSCRIBER_CAPACITY);
        publisher
            .connect_to_subscriber(&mut subscriber)
            .unwrap_or_else(|_| {
                panic!("Type mismatch connecting TestSubscriber to channel '{channel_name}'")
            });
        subscriber
    }

    /// Wires up any remaining real connections (and allocates the callbacks' own publisher
    /// arenas, now correctly sized — test connections above already bumped capacities where
    /// needed), then constructs the executor.
    pub fn build(mut self) -> UnitTestExecutor {
        connect_callbacks(&mut self.tasks).unwrap_or_else(|e| {
            panic!("Failed to connect callbacks: {e}");
        });

        let pools = vec![ThreadPoolConfig {
            thread_count: 1,
            tasks: self.tasks,
        }];
        UnitTestExecutor::new_with_time_cell(
            UnitTestExecutorConfig {
                start_time: self.start_time,
                pools,
                callback_executor_thread_count: 1,
            },
            Some(self.current_time),
        )
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
        let mut integer_publisher = builder.add_test_publisher::<u64>(0, "integer");
        let mut string_subscriber = builder.add_test_subscriber::<String>(0, "fizz_buzz_string");
        let mut executor = builder.build();

        integer_publisher.send(15);
        executor.step();

        let messages = string_subscriber.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message, "FizzBuzz");
    }
}
