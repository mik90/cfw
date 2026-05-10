use crate::executor;
use crate::state::SimulationState;
use crate::{SimulationConfig, TaskIndex};
use std::num::Saturating;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use task::callback::ConnectedCallback;
use task::executor::{Executor, ExecutorError, ExecutorStopSignal, ThreadPoolConfig};
use task::time::FrameworkTime;

pub struct StopSignal(Arc<AtomicBool>);

impl ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct SimulationExecutor {
    // Other threads may swap this on/off to stop
    should_run: Arc<AtomicBool>,

    state: Arc<Mutex<SimulationState>>,

    /// Background thread running the step loop. Present after start(), absent before.
    step_thread: Option<JoinHandle<()>>,
}

impl SimulationExecutor {
    /// Create a single virtual pool with `num_virtual_threads` for all tasks,
    /// starting at simulation time zero
    pub fn new(num_virtual_threads: usize, tasks: Vec<ConnectedCallback>) -> Self {
        Self::new_with(SimulationConfig {
            // We can't create an instant from a fixed value, so any 'now' will be arbitrary
            start_time: FrameworkTime::from_wall_clock(),
            pools: vec![ThreadPoolConfig::new(num_virtual_threads, tasks)],
            callback_executor_thread_count: 1,
        })
    }

    /// Create an executor from a [`SimulationConfig`], supporting multiple virtual
    /// pools and a configurable start time.
    pub fn new_with(config: SimulationConfig) -> Self {
        let should_run = Arc::new(AtomicBool::new(false));

        SimulationExecutor {
            should_run,
            state: Arc::new(Mutex::new(SimulationState::new_with(config))),
            step_thread: None,
        }
    }

    /// Block until the step thread exits on its own (e.g. a callback fired the stop signal).
    /// Use this when you want to wait for natural completion without forcing a stop.
    /// Call [`stop`] afterwards to join the callback executor threads.
    pub fn join(&mut self) -> Result<(), ExecutorError> {
        if let Some(t) = self.step_thread.take() {
            if t.join().is_err() {
                return Err(ExecutorError::PanickedThreads(vec![0]));
            }
        }
        Ok(())
    }

    /// Run a single simulation step on the caller's thread. The caller is responsible
    /// for any one-time setup (see [`SimulationState::start`]) before the first call,
    /// and for cleanup once stepping is done.
    pub fn step(&mut self) -> Vec<TaskIndex> {
        self.state.lock().unwrap().step()
    }

    pub fn get_step_count(&self) -> Saturating<usize> {
        self.state.lock().unwrap().get_step_count()
    }

    pub fn get_simulation_time(&self) -> FrameworkTime {
        self.state.lock().unwrap().get_simulation_time()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use task::{executor::Executor, test_tasks::*};

    use super::SimulationExecutor;

    #[test]
    fn test_simulation_exec() {
        let (callbacks, task_info) = build_fizz_buzz_tasks();

        let mut exec = SimulationExecutor::new(1, callbacks);

        task_info.stop_signal.set(exec.stop_signal()).ok();
        // start() spawns the step loop thread and returns immediately.
        exec.start();
        // join() blocks until a callback fires the stop signal and the thread exits.
        assert!(exec.join().is_ok());
        // stop() shuts down the callback executor threads.
        let stop_result = exec.stop();
        assert!(stop_result.is_ok());

        assert!(!exec.is_running());
        assert!(!task_info.get_stored_strings().is_empty());
    }

    #[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
    struct StepState {
        tasks_executed: Vec<usize>,
        offset_from_start: Duration,
        string_store: Vec<String>,
    }

    fn run_fizz_buzz_for_n_steps(step_count: usize, thread_count: usize) -> Vec<StepState> {
        let (callbacks, task_info) = build_fizz_buzz_tasks();

        let exec = SimulationExecutor::new(thread_count, callbacks);
        let mut state = exec.state.lock().unwrap();
        let start_time = state.get_simulation_time();
        state.start();

        let mut step_history = vec![];
        for _ in 0..step_count {
            let maybe_offset = state
                .get_simulation_time()
                .checked_duration_since(start_time);
            assert!(maybe_offset.is_some());
            let tasks_executed = state.step();
            step_history.push(StepState {
                tasks_executed,
                offset_from_start: maybe_offset.unwrap(),
                string_store: task_info.get_stored_strings(),
            });
        }
        step_history
    }

    #[test]
    fn test_determinism() {
        let history_first = run_fizz_buzz_for_n_steps(50, 2);
        let history_second = run_fizz_buzz_for_n_steps(50, 2);
        assert_eq!(history_first, history_second);
    }

    /// Verify that fair (longest-wait-first) scheduling prevents starvation.
    ///
    /// Three tasks compete for a single virtual thread. Each task re-schedules itself
    /// for the instant it finishes (period = 0), so all three are always simultaneously
    /// ready. Without fair scheduling, task 0 would win every step due to index order;
    /// tasks 1 and 2 would never run. With fair scheduling, tasks 1 and 2 are served
    /// first on steps 2 and 3 because they have been waiting since t=0 while task 0
    /// only became ready again at t=1ms.
    #[test]
    fn test_no_starvation() {
        let callbacks = (0..3).map(|_| build_no_op_callback()).collect();

        let exec = SimulationExecutor::new(1, callbacks); // 1 virtual thread, 3 tasks
        let mut state = exec.state.lock().unwrap();
        state.start();

        let mut run_counts = vec![0usize; 3];
        for _ in 0..6 {
            for idx in state.step() {
                run_counts[idx] += 1;
            }
        }

        for (i, &count) in run_counts.iter().enumerate() {
            assert!(count > 0, "task {i} never ran — starvation detected");
        }

        // Each task should have run roughly equally (within 1 of each other),
        // since they are identical and always simultaneously ready.
        let min = *run_counts.iter().min().unwrap();
        let max = *run_counts.iter().max().unwrap();
        assert!(
            max - min <= 1,
            "tasks ran unequally: {run_counts:?} — scheduling is unfair"
        );
    }
}

impl Executor for SimulationExecutor {
    /// Spawns a background thread that steps the simulation until something flips
    /// the stop signal (e.g. a callback calling [`ExecutorStopSignal::request_stop`]).
    /// Returns immediately; call [`stop`] to join the thread.
    fn start(&mut self) {
        let should_run = self.should_run.clone();
        let state = self.state.clone();

        should_run.store(true, Ordering::Release);
        state.lock().unwrap().start();

        self.step_thread = Some(thread::spawn(move || {
            while should_run.load(Ordering::Acquire) {
                state.lock().unwrap().step();
            }
            state.lock().unwrap().cleanup();
        }));
    }

    fn stop(&mut self) -> Result<(), ExecutorError> {
        self.should_run.store(false, Ordering::Release);
        // Join the step thread before shutting down callback threads, so we don't
        // pull the rug out from under an in-progress step.
        if let Some(t) = self.step_thread.take() {
            if t.join().is_err() {
                return Err(ExecutorError::PanickedThreads(vec![0]));
            }
        }
        match self.state.lock().unwrap().shutdown_callback_threads() {
            Ok(()) => Ok(()),
            Err(idxs) => Err(ExecutorError::PanickedThreads(idxs)),
        }
    }

    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal> {
        Arc::new(StopSignal(self.should_run.clone()))
    }

    fn is_running(&self) -> bool {
        self.should_run.load(Ordering::Acquire)
    }
}
