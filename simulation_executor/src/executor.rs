use crate::state::SimulationState;
pub use crate::state::StepError;
use crate::{CallbackNodeIndex, SimulationConfig};
use std::num::Saturating;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use task::callback_storage::CallbackStorage;
use task::executor::{Executor, ExecutorParams, ExecutorStopSignal, ThreadPoolConfig};
use task::time::FrameworkTime;

#[derive(Debug)]
pub enum SimulationExecutorError {
    StepThreadPanicked,
    NodeExecutorThreadsPanicked(Vec<usize>),
}

impl std::fmt::Display for SimulationExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SimulationExecutorError::StepThreadPanicked => write!(f, "step thread panicked"),
            SimulationExecutorError::NodeExecutorThreadsPanicked(idxs) => {
                write!(f, "node executor threads panicked: {idxs:?}")
            }
        }
    }
}

impl std::error::Error for SimulationExecutorError {}

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

    /// Error produced by the step loop, if any. Set by the step thread; read by join().
    step_error: Arc<Mutex<Option<StepError>>>,
}

impl SimulationExecutor {
    /// Create a single virtual pool with `num_virtual_threads` for all callback nodes,
    /// starting at simulation time zero
    pub fn new(num_virtual_threads: usize, nodes: impl Into<CallbackStorage>) -> Self {
        Self::new_with(SimulationConfig {
            // We can't create an instant from a fixed value, so any 'now' will be arbitrary
            start_time: FrameworkTime::from_wall_clock(),
            executor_params: ExecutorParams::new(vec![ThreadPoolConfig::new(
                num_virtual_threads,
                nodes,
            )]),
            node_executor_thread_count: 1,
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
            step_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Block until the step thread exits on its own (e.g. a callback node fired the stop signal).
    /// Use this when you want to wait for natural completion without forcing a stop.
    /// Call [`stop`] afterwards to join the node executor threads.
    /// Returns the step error if the loop stopped due to one.
    pub fn join(&mut self) -> Result<(), StepError> {
        if let Some(t) = self.step_thread.take()
            && t.join().is_err()
        {
            return Err(StepError::StepThreadPanicked);
        }
        if let Some(e) = self.step_error.lock().unwrap().take() {
            return Err(e);
        }
        Ok(())
    }

    /// Run a single simulation step on the caller's thread. The caller is responsible
    /// for any one-time setup (see [`SimulationState::start`]) before the first call,
    /// and for cleanup once stepping is done.
    pub fn step(&mut self) -> Result<Vec<CallbackNodeIndex>, StepError> {
        self.state.lock().unwrap().step()
    }

    pub fn step_count(&self) -> Saturating<usize> {
        self.state.lock().unwrap().step_count()
    }

    pub fn simulation_time(&self) -> FrameworkTime {
        self.state.lock().unwrap().simulation_time()
    }
}

impl Executor for SimulationExecutor {
    type Error = SimulationExecutorError;

    /// Spawns a background thread that steps the simulation until something flips
    /// the stop signal (e.g. a callback node calling [`ExecutorStopSignal::request_stop`]).
    /// Returns immediately; call [`stop`] to join the thread.
    fn start(&mut self) {
        let should_run = self.should_run.clone();
        let state = self.state.clone();
        let step_error = self.step_error.clone();

        should_run.store(true, Ordering::Release);
        state.lock().unwrap().start();

        self.step_thread = Some(thread::spawn(move || {
            while should_run.load(Ordering::Acquire) {
                if let Err(e) = state.lock().unwrap().step() {
                    should_run.store(false, Ordering::Release);
                    *step_error.lock().unwrap() = Some(e);
                    break;
                }
            }
            state.lock().unwrap().cleanup();
        }));
    }

    fn stop(&mut self) -> Result<(), SimulationExecutorError> {
        self.should_run.store(false, Ordering::Release);
        // Join the step thread before shutting down node executor threads, so we don't
        // pull the rug out from under an in-progress step.
        if let Some(t) = self.step_thread.take()
            && t.join().is_err()
        {
            return Err(SimulationExecutorError::StepThreadPanicked);
        }
        match self.state.lock().unwrap().shutdown_node_executor_threads() {
            Ok(()) => Ok(()),
            Err(idxs) => Err(SimulationExecutorError::NodeExecutorThreadsPanicked(idxs)),
        }
    }

    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal> {
        Arc::new(StopSignal(self.should_run.clone()))
    }

    fn is_running(&self) -> bool {
        self.should_run.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use task::executor::Executor;
    use test_tasks::*;

    use super::SimulationExecutor;

    #[test]
    fn test_simulation_exec() {
        let (nodes, task_info) = build_fizz_buzz_callback_nodes();

        let mut exec = SimulationExecutor::new(1, nodes);

        task_info.stop_signal.set(exec.stop_signal()).ok();
        // start() spawns the step loop thread and returns immediately.
        exec.start();
        // join() blocks until a callback node fires the stop signal and the thread exits.
        assert!(exec.join().is_ok());
        // stop() shuts down the node executor threads.
        let stop_result = exec.stop();
        assert!(stop_result.is_ok());

        assert!(!exec.is_running());
        assert!(!task_info.stored_strings().is_empty());
    }

    #[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
    struct StepState {
        nodes_executed: Vec<usize>,
        offset_from_start: Duration,
        string_store: Vec<String>,
    }

    fn run_fizz_buzz_for_n_steps(step_count: usize, thread_count: usize) -> Vec<StepState> {
        let (nodes, task_info) = build_fizz_buzz_callback_nodes();

        let exec = SimulationExecutor::new(thread_count, nodes);
        let mut state = exec.state.lock().unwrap();
        let start_time = state.simulation_time();
        state.start();

        let mut step_history = vec![];
        for _ in 0..step_count {
            let maybe_offset = state.simulation_time().checked_duration_since(start_time);
            assert!(maybe_offset.is_some());
            let nodes_executed = state.step().unwrap();
            step_history.push(StepState {
                nodes_executed,
                offset_from_start: maybe_offset.unwrap(),
                string_store: task_info.stored_strings(),
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
    /// Three callback nodes compete for a single virtual thread. Each node re-schedules itself
    /// for the instant it finishes (period = 0), so all three are always simultaneously
    /// ready. Without fair scheduling, node 0 would win every step due to index order;
    /// nodes 1 and 2 would never run. With fair scheduling, nodes 1 and 2 are served
    /// first on steps 2 and 3 because they have been waiting since t=0 while node 0
    /// only became ready again at t=1ms.
    #[test]
    fn test_no_starvation() {
        let nodes: Vec<_> = (0..3).map(|_| build_no_op_callback_node()).collect();

        let exec = SimulationExecutor::new(1, nodes); // 1 virtual thread, 3 callback nodes
        let mut state = exec.state.lock().unwrap();
        state.start();

        let mut run_counts = vec![0usize; 3];
        for _ in 0..6 {
            for idx in state.step().unwrap() {
                run_counts[idx] += 1;
            }
        }

        for (i, &count) in run_counts.iter().enumerate() {
            assert!(
                count > 0,
                "callback node {i} never ran — starvation detected"
            );
        }

        // Each callback node should have run roughly equally (within 1 of each other),
        // since they are identical and always simultaneously ready.
        let min = *run_counts.iter().min().unwrap();
        let max = *run_counts.iter().max().unwrap();
        assert!(
            max - min <= 1,
            "callback nodes ran unequally: {run_counts:?} — scheduling is unfair"
        );
    }
}
