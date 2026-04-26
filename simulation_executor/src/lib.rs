use rayon::prelude::*;
use std::collections::VecDeque;
use std::num::Saturating;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use task::callback::ConnectedCallback;
use task::context::Context;
use task::executor::{Executor, ExecutorError, ExecutorStopSignal, ThreadPoolConfig};
use task::time::FrameworkTime;

#[derive(Clone, Copy)]
struct TimeTriggeredTask {
    index: usize,
    requested_exec_time: FrameworkTime,
}

pub struct StopSignal(Arc<AtomicBool>);

/// A virtual pool tracks how many concurrent "threads" it models,
/// without spawning real OS threads.
struct VirtualPool {
    /// Total count of threads in the pool
    virtual_thread_count: usize,

    /// How many threads are 'taken up' by a task until its busy_until time is reached
    num_threads_occupied: usize,
}

pub struct SimulationExecutor {
    // Threads that run tasks, not the same as the virtual threads that model live threading behavior
    execution_threads: Vec<thread::JoinHandle<()>>,

    // Other threads may swap this on/off to stop
    should_run: Arc<AtomicBool>,

    state: Arc<Mutex<SimulationState>>,
}

type PoolIndex = usize;
type TaskIndex = usize;

struct SimulationState {
    /// Storage of all tasks. A task's index into this vec is used to index into other Vecs.
    /// Each task is wrapped in a Mutex so the parallel execution block can lock individual
    /// tasks without unsafe disjoint-index tricks.
    tasks: Vec<Mutex<ConnectedCallback>>,

    /// Thread pool used to run tasks in parallel within a single simulation step.
    /// Its size is independent of any virtual thread pool size.
    thread_pool: rayon::ThreadPool,

    /// Maps each global task index to its pool index
    task_to_pool: Vec<PoolIndex>,

    /// Virtual pools — no real threads, but models concurrency boundaries
    pools: Vec<VirtualPool>,

    /// TODO should this be a sorted queue?
    periodic_tasks: VecDeque<TimeTriggeredTask>,

    /// Per-task sim-time when each task's last execution finishes. Initialized to start_time.
    task_busy_until: Vec<FrameworkTime>,

    /// Sim-time when each task first became ready but hadn't yet been allocated a thread.
    /// None if the task is not currently waiting. Used to prioritize longest-waiting tasks.
    task_ready_since: Vec<Option<FrameworkTime>>,

    /// Current simulation time
    time: FrameworkTime,

    /// Number of times the state has been stepped
    step_count: Saturating<usize>,
}

impl ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Executor for SimulationExecutor {
    fn start(&mut self) {
        self.should_run.store(true, Ordering::Release);
        let state = self.state.clone();
        let should_run = self.should_run.clone();
        self.execution_threads.push(thread::spawn(move || {
            state.lock().unwrap().start();
            while should_run.load(Ordering::Acquire) {
                state.lock().unwrap().step();
            }
            state.lock().unwrap().cleanup();
        }));
    }

    fn stop(&mut self) -> Result<(), ExecutorError> {
        self.should_run.store(false, Ordering::Release);

        let mut thread_join_result = vec![];
        println!("Joining {} threads", self.execution_threads.len());
        for (thread_idx, t) in self.execution_threads.drain(..).enumerate() {
            match t.join() {
                Ok(()) => {}
                Err(_) => {
                    thread_join_result.push(thread_idx);
                }
            }
            println!("joined thread {thread_idx}");
        }

        if thread_join_result.is_empty() {
            return Ok(());
        }
        Err(ExecutorError::PanickedThreads(thread_join_result))
    }

    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal> {
        Arc::new(StopSignal(self.should_run.clone()))
    }

    fn is_running(&self) -> bool {
        self.should_run.load(Ordering::Acquire)
    }
}

pub struct SimulationConfig {
    pub start_time: FrameworkTime,
    pub pools: Vec<ThreadPoolConfig>,
    /// Number of real OS threads used to execute tasks in parallel within a step.
    /// Independent of any virtual thread pool sizes.
    pub execution_thread_count: usize,
}

impl SimulationExecutor {
    /// Create a single virtual pool with `num_virtual_threads` for all tasks,
    /// starting at simulation time zero
    pub fn new(num_virtual_threads: usize, tasks: Vec<ConnectedCallback>) -> Self {
        Self::new_with(SimulationConfig {
            // We can't create an instant from a fixed value, so any 'now' will be arbitrary
            start_time: FrameworkTime::now(),
            pools: vec![ThreadPoolConfig::new(num_virtual_threads, tasks)],
            execution_thread_count: 1,
        })
    }

    /// Create an executor from a [`SimulationConfig`], supporting multiple virtual
    /// pools and a configurable start time.
    pub fn new_with(config: SimulationConfig) -> Self {
        let mut all_tasks: Vec<Mutex<ConnectedCallback>> = Vec::new();
        let mut task_to_pool: Vec<usize> = Vec::new();
        let mut virtual_pools: Vec<VirtualPool> = Vec::new();

        for (pool_idx, pool) in config.pools.into_iter().enumerate() {
            virtual_pools.push(VirtualPool {
                virtual_thread_count: pool.thread_count,
                num_threads_occupied: 0,
            });
            for task in pool.tasks {
                task_to_pool.push(pool_idx);
                all_tasks.push(Mutex::new(task));
            }
        }

        let num_tasks = all_tasks.len();
        let thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.execution_thread_count)
            .build()
            .expect("failed to build rayon thread pool");
        let should_run = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(SimulationState {
            tasks: all_tasks,
            thread_pool,
            task_to_pool,
            pools: virtual_pools,
            periodic_tasks: VecDeque::new(),
            task_busy_until: vec![config.start_time; num_tasks],
            task_ready_since: vec![None; num_tasks],
            time: config.start_time,
            step_count: Saturating(0),
        }));

        SimulationExecutor {
            execution_threads: Vec::new(),
            should_run,
            state,
        }
    }

    pub fn get_step_count(&self) -> Saturating<usize> {
        self.state.lock().unwrap().get_step_count()
    }

    pub fn get_simulation_time(&self) -> FrameworkTime {
        self.state.lock().unwrap().get_simulation_time()
    }
}

impl SimulationState {
    pub fn start(&mut self) {
        // Set up periodic execution
        for (index, callback) in self.tasks.iter().enumerate() {
            if let Some(_) = callback
                .lock()
                .unwrap()
                .get_next_requested_execution_time(self.time)
            {
                self.periodic_tasks.push_back(TimeTriggeredTask {
                    index,
                    // Periodic tasks will run on startup, and then their requested times will be honored
                    requested_exec_time: self.time,
                });
            }
        }
    }

    /// Finds tasks that should run this step, allocates a thread from their pool to each,
    /// and drains their subscribers (write → read) so data is available when they run.
    ///
    /// A task is a candidate if it has new trigger data in its write buffer and isn't busy,
    /// or it is a periodic task that is due and isn't busy. Among candidates, only those
    /// whose pool has a free thread are returned. Drain is deferred until after thread
    /// allocation so that tasks which can't run don't consume their trigger data.
    fn allocate_tasks_to_threads(&mut self) -> Vec<TaskIndex> {
        let mut candidates: Vec<TaskIndex> = vec![];

        for index in 0..self.tasks.len() {
            if self.tasks[index]
                .lock()
                .unwrap()
                .subscribers_request_execution()
                && self.time >= self.task_busy_until[index]
            {
                candidates.push(index);
            }
        }
        for periodic in &self.periodic_tasks {
            if periodic.requested_exec_time <= self.time
                && self.time >= self.task_busy_until[periodic.index]
                && !candidates.contains(&periodic.index)
            {
                candidates.push(periodic.index);
            }
        }

        // Record when each task first became ready, then sort by wait time (oldest first)
        // with task index as a tiebreaker to preserve determinism.
        for &index in &candidates {
            self.task_ready_since[index].get_or_insert(self.time);
        }
        candidates.sort_by_key(|&index| (self.task_ready_since[index], index));

        let mut runnable: Vec<TaskIndex> = vec![];
        for index in candidates {
            let pool_index = self.task_to_pool[index];
            let pool = &mut self.pools[pool_index];
            if pool.num_threads_occupied < pool.virtual_thread_count {
                pool.num_threads_occupied += 1;
                self.task_ready_since[index] = None;
                runnable.push(index);
            }
        }

        runnable
    }

    pub fn step(&mut self) -> Vec<TaskIndex> {
        println!("Start step {}", self.step_count);

        let runnable_tasks = self.allocate_tasks_to_threads();
        // Only drain subscribers for tasks that actually got a thread, so that tasks
        // blocked by pool pressure keep their trigger data for the next step.
        for &index in &runnable_tasks {
            self.tasks[index].lock().unwrap().drain_subscribers();
        }

        let time = self.time;
        let durations: Vec<(TaskIndex, std::time::Duration)> = self.thread_pool.install(|| {
            runnable_tasks
                .par_iter()
                .map(|&index| {
                    let ctx = Context::new(time);
                    let mut task = self.tasks[index].lock().unwrap();
                    task.run(&ctx);
                    (index, task.get_execution_duration())
                })
                .collect()
        });
        for (index, duration) in durations {
            self.task_busy_until[index] = time + duration;
        }

        for &index in &runnable_tasks {
            self.tasks[index].lock().unwrap().flush_publishers(time);
        }

        // Update periodic task next-run times from their no-longer-busy instant
        for periodic in &mut self.periodic_tasks {
            if runnable_tasks.contains(&periodic.index) {
                let no_longer_busy = self.task_busy_until[periodic.index];
                if let Some(next_time) = self.tasks[periodic.index]
                    .lock()
                    .unwrap()
                    .get_next_requested_execution_time(no_longer_busy)
                {
                    periodic.requested_exec_time = next_time;
                }
            }
        }

        let old_sim_time = self.time;

        // Advance sim time to earliest next event
        let next_busy = runnable_tasks
            .iter()
            .map(|&i| self.task_busy_until[i])
            .min();
        let next_periodic = self
            .periodic_tasks
            .iter()
            .map(|p| p.requested_exec_time)
            .filter(|&t| t > self.time)
            .min();
        if let Some(t) = [next_busy, next_periodic].into_iter().flatten().min() {
            if t > self.time {
                self.time = t;
            }
        }

        // See if any tasks are no longer busy, and if they aren't, free up a thread from their pool
        for (index, t) in self.task_busy_until.iter().enumerate() {
            let busy_until_time = *t;
            if busy_until_time > old_sim_time && busy_until_time <= self.time {
                // task is no longer busy as of the new sim time, so we can free up a thread
                let pool_index = self.task_to_pool[index];
                self.pools[pool_index].num_threads_occupied -= 1;
            }
        }
        println!("End step {}", self.step_count);
        self.step_count += 1;
        runnable_tasks
    }

    pub fn get_step_count(&self) -> Saturating<usize> {
        self.step_count
    }

    pub fn get_simulation_time(&self) -> FrameworkTime {
        self.time
    }

    pub fn cleanup(&mut self) {
        // Clean up subscriber buffers so we can destroy publisher message storage
        for task in self.tasks.iter() {
            for subscriber in task.lock().unwrap().get_subscribers().iter() {
                subscriber.cleanup_buffers();
            }
        }
    }
}

impl Drop for SimulationState {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex, OnceLock},
        thread::sleep,
        time::{self, Duration, Instant},
    };

    use task::{executor::Executor, test_tasks::*};

    use crate::{SimulationExecutor, SimulationState};

    #[test]
    fn test_simulation_exec() {
        let (callbacks, task_info) = build_fizz_buzz_tasks();

        let mut exec = SimulationExecutor::new(1, callbacks);

        task_info.stop_signal.set(exec.stop_signal()).ok();
        exec.start();

        let deadline = time::Instant::now() + time::Duration::from_secs(10);
        while exec.is_running() && time::Instant::now() < deadline {
            sleep(time::Duration::from_millis(10));
        }
        assert!(
            !exec.is_running(),
            "Executor did not stop itself within 10 seconds"
        );

        let stop_result = exec.stop();
        assert!(stop_result.is_ok());

        assert!(!task_info.get_stored_strings().is_empty());
    }

    /// Lower level test that manually steps sim tsate
    #[test]
    fn test_simulation_state() {
        let (callbacks, task_info) = build_fizz_buzz_tasks();

        let exec = SimulationExecutor::new(1, callbacks);
        let mut state = exec.state.lock().unwrap();
        state.start();
        let start_time = state.get_simulation_time();

        assert_eq!(state.tasks.len(), 3);

        let periodic = state.periodic_tasks.front().unwrap();
        assert_eq!(periodic.index, 0);
        assert_eq!(periodic.requested_exec_time, start_time);

        let executed_tasks = state.step();
        assert_eq!(executed_tasks, vec![task_info.integer_publisher_index]);

        // After first step, the publisher should want to run in the future
        let periodic = state.periodic_tasks.front().unwrap();
        assert_eq!(periodic.index, 0);
        assert_eq!(
            periodic.requested_exec_time,
            start_time + Duration::from_millis(1) + Duration::from_millis(500),
            "Publisher task takes 1ms to run and wants to run every 500ms"
        );

        let executed_tasks = state.step();
        assert_eq!(
            executed_tasks,
            vec![task_info.fizz_buzz_index],
            "After the second step, the fizz-buzz task should have run"
        );

        let executed_tasks = state.step();
        assert_eq!(
            executed_tasks,
            vec![task_info.string_store_index],
            "After the third step, the string store task should've run"
        );

        assert_eq!(task_info.get_stored_strings(), vec!["FizzBuzz"]);

        println!("Done");
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
