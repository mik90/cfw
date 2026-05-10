use std::fmt;
use std::num::Saturating;

use simulation_executor::SimulationConfig;
use simulation_executor::state::SimulationState;
use task::callback::ConnectedCallback;
use task::executor::ThreadPoolConfig;
use task::time::FrameworkTime;

/// Struct for running unit tests against tasks
pub struct TaskTest {
    simulation_state: SimulationState,
}

impl TaskTest {
    /// Create simple task tester
    pub fn new(tasks: Vec<ConnectedCallback>) -> Self {
        let pools = vec![ThreadPoolConfig {
            thread_count: 1,
            tasks: tasks,
        }];
        Self::new_with(TaskTestConfig {
            start_time: FrameworkTime::from_nanoseconds(0),
            pools,
            callback_executor_thread_count: 1,
        })
    }

    /// Create task tester with custom config
    pub fn new_with(config: TaskTestConfig) -> Self {
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
    pub fn step(&mut self) -> StepResult {
        let before = self.simulation_state.get_simulation_time();
        self.simulation_state.step();
        let after = self.simulation_state.get_simulation_time();
        StepResult { before, after }
    }

    pub fn get_step_count(&self) -> Saturating<usize> {
        self.simulation_state.get_step_count()
    }

    pub fn get_current_time(&self) -> FrameworkTime {
        self.simulation_state.get_simulation_time()
    }
}

/// Configuration for running tasks
pub struct TaskTestConfig {
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
mod tests {}
