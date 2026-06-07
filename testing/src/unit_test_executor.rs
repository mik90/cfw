use std::fmt;
use std::num::Saturating;

use simulation_executor::SimulationConfig;
use simulation_executor::state::{SimulationState, StepError};
use task::callback::ConnectedCallback;
use task::executor::ThreadPoolConfig;
use task::time::FrameworkTime;

/// Struct for running unit tests against tasks
pub struct UnitTestExecutor {
    simulation_state: SimulationState,
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
        let mut task_test = Self {
            simulation_state: SimulationState::new_with(SimulationConfig {
                start_time: config.start_time,
                pools: config.pools,
                callback_executor_thread_count: config.callback_executor_thread_count,
            }),
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
        Ok(StepResult { before, after })
    }

    pub fn get_step_count(&self) -> Saturating<usize> {
        self.simulation_state.get_step_count()
    }

    pub fn get_current_time(&self) -> FrameworkTime {
        self.simulation_state.get_simulation_time()
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
    use task::test_tasks::*;

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
    }
}
