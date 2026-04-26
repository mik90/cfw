use crossbeam::channel::{self, Receiver, Sender};
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{self, Duration};
use task::time::FrameworkTime;

use task::callback::ConnectedCallback;
use task::context::Context;
use task::executor::{Executor, ExecutorError, ExecutorStopSignal, TaskEnqueuer, ThreadPoolConfig};

/// Sent into a pool's work channel to unblock workers on shutdown.
const SHUTDOWN_SENTINEL: usize = usize::MAX;

#[derive(Clone, Copy)]
struct TimeTriggeredTask {
    index: usize,
    requested_exec_time: FrameworkTime,
}

struct PoolState {
    thread_count: usize,
    work_tx: Sender<usize>,
    work_rx: Receiver<usize>,
}

struct SharedThreadPoolState {
    /// One entry per logical thread pool
    pools: Vec<Arc<PoolState>>,

    /// Maps each global task index to its pool index
    task_to_pool: Vec<usize>,

    /// One flag per task: true while the task index is sitting in the work channel.
    /// CAS'd to true on enqueue, cleared to false before execution begins, preventing
    /// duplicate entries in the channel.
    task_enqueued: Vec<AtomicBool>,

    /// Mutex and condvar used only to sleep/wake the periodic trigger thread
    periodic_mutex: Mutex<()>,
    periodic_cond_var: Condvar,

    /// Storage of all tasks across all pools - each task has its own mutex
    /// for fine-grained locking, allowing concurrent execution across pools.
    tasks: Vec<Arc<Mutex<ConnectedCallback>>>,

    /// Whether the thread pool should continue running
    should_run: AtomicBool,
}

impl SharedThreadPoolState {
    fn trigger_task(&self, index: usize) {
        // Only enqueue if the task isn't already queued
        if self.task_enqueued[index]
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let pool = &self.pools[self.task_to_pool[index]];
            let _ = pool.work_tx.send(index);
            println!("Triggering task index {}", index);
        }
    }
}

impl fmt::Display for SharedThreadPoolState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Should run: {}", self.should_run.load(Ordering::Relaxed))?;
        writeln!(f, "All tasks:")?;
        for (index, arc_task) in self.tasks.iter().enumerate() {
            let task = arc_task.lock().unwrap();
            writeln!(f, "\t ----------------------------------")?;
            writeln!(
                f,
                "\t Index:{}, Name: {}, Pool: {}",
                index,
                task.get_name(),
                self.task_to_pool[index]
            )?;
            writeln!(f, "\t Able to run: {}", task.able_to_run())?;
            writeln!(
                f,
                "\t Subscribers request execution: {}",
                task.subscribers_request_execution()
            )?;
            writeln!(f, "\t Subscribers")?;
            for s in task.get_subscribers().iter() {
                writeln!(f, "\t\t Channel: {}", s.get_config().channel_name)?;
                let queue_info = s.get_queue_info();
                writeln!(
                    f,
                    "\t\t Reader queue size: {}, writer_queue size: {}",
                    queue_info.reader_size, queue_info.writer_size
                )?;
            }
        }
        writeln!(f, "\t ----------------------------------")?;
        Ok(())
    }
}

pub struct LiveExecutor {
    threads: Vec<thread::JoinHandle<()>>,
    shared_state: Arc<SharedThreadPoolState>,
}

fn periodic_trigger_thread(
    shared_state: &SharedThreadPoolState,
    exec_times: &mut VecDeque<TimeTriggeredTask>,
) {
    let now = task::time::FrameworkTime::from_wall_clock();

    let maybe_earliest = exec_times
        .iter()
        .min_by_key(|task| task.requested_exec_time)
        .copied();

    let time_triggered_task = match maybe_earliest {
        Some(t) => t,
        None => {
            println!("No task had a periodic trigger");
            // No timed tasks — wait until stopped
            let guard = shared_state.periodic_mutex.lock().unwrap();
            drop(shared_state.periodic_cond_var.wait(guard));
            return;
        }
    };

    if let Some(duration) = time_triggered_task
        .requested_exec_time
        .checked_duration_since(now)
    {
        // Wait until it's time to run the earliest task, or until woken early (e.g. shutdown)
        let guard = shared_state.periodic_mutex.lock().unwrap();
        let _ = shared_state
            .periodic_cond_var
            .wait_timeout(guard, duration)
            .unwrap();
    }

    if now <= time_triggered_task.requested_exec_time {
        shared_state.trigger_task(time_triggered_task.index);

        let task_guard = shared_state.tasks[time_triggered_task.index]
            .lock()
            .unwrap();

        let next_exec_time = task_guard
            .get_next_requested_execution_time(now)
            .unwrap_or(task::time::FrameworkTime::MAX);

        for t in exec_times.iter_mut() {
            if t.index == time_triggered_task.index {
                t.requested_exec_time = next_exec_time;
            }
        }
    }
}

fn executor_cycle(pool_state: &PoolState, shared_state: &SharedThreadPoolState) {
    let index = match pool_state.work_rx.recv() {
        Ok(idx) if idx == SHUTDOWN_SENTINEL => return,
        Ok(idx) => idx,
        Err(_) => return, // channel disconnected
    };

    if !shared_state.should_run.load(Ordering::Relaxed) {
        return;
    }

    // Clear the enqueued flag before running so any triggers that arrive during
    // execution are captured, not dropped
    shared_state.task_enqueued[index].store(false, Ordering::Release);

    let mut task_guard = shared_state.tasks[index].lock().unwrap();
    println!("Found runnable task {}", task_guard.get_name());
    let ctx = Context::new(task::time::FrameworkTime::from_wall_clock());
    task_guard.drain_subscribers();
    let _ = task_guard.run(&ctx);
    task_guard.flush_publishers(ctx.now);
    println!("Finished running {}", task_guard.get_name());
}

impl LiveExecutor {
    /// Create a single shared thread pool with `num_threads` workers for all tasks.
    pub fn new(num_threads: usize, tasks: Vec<ConnectedCallback>) -> Self {
        Self::new_multi_pool(vec![ThreadPoolConfig::new(num_threads, tasks)])
    }

    /// Create multiple independent thread pools in one executor.
    ///
    /// Tasks in different pools execute on separate worker threads, enabling
    /// priority separation: put latency-sensitive tasks in a pool with dedicated
    /// threads and background tasks in another. The periodic trigger thread and
    /// task storage are shared across all pools to minimise overhead.
    pub fn new_multi_pool(pools: Vec<ThreadPoolConfig>) -> Self {
        let mut all_arc_tasks: Vec<Arc<Mutex<ConnectedCallback>>> = Vec::new();
        let mut task_to_pool: Vec<usize> = Vec::new();
        let mut pool_states: Vec<Arc<PoolState>> = Vec::new();

        for (pool_idx, pool) in pools.into_iter().enumerate() {
            // Channel capacity: one slot per task in this pool (AtomicBool dedup ensures
            // at most one entry per task) plus one per worker thread for shutdown sentinels
            let capacity = pool.tasks.len() + pool.thread_count;
            let (work_tx, work_rx) = channel::bounded(capacity.max(1));

            pool_states.push(Arc::new(PoolState {
                thread_count: pool.thread_count,
                work_tx,
                work_rx,
            }));

            for task in pool.tasks {
                task_to_pool.push(pool_idx);
                all_arc_tasks.push(Arc::new(Mutex::new(task)));
            }
        }

        let num_tasks = all_arc_tasks.len();
        let shared_state = Arc::new(SharedThreadPoolState {
            pools: pool_states,
            task_to_pool,
            task_enqueued: (0..num_tasks).map(|_| AtomicBool::new(false)).collect(),
            periodic_mutex: Mutex::new(()),
            periodic_cond_var: Condvar::new(),
            tasks: all_arc_tasks,
            should_run: true.into(),
        });

        let enqueuer = shared_state.clone() as Arc<dyn TaskEnqueuer>;
        for (index, arc_task) in shared_state.tasks.iter().enumerate() {
            arc_task
                .lock()
                .unwrap()
                .register_with_executor(index, enqueuer.clone());
        }

        LiveExecutor {
            threads: Vec::new(),
            shared_state,
        }
    }

    pub fn start_threads(&mut self) {
        // Seed each pool's work queue for tasks that want to start immediately
        for index in 0..self.shared_state.tasks.len() {
            let task = self.shared_state.tasks[index].lock().unwrap();
            if task.subscribers_request_execution() && task.able_to_run() {
                drop(task);
                self.shared_state.trigger_task(index);
            }
        }

        // Spawn worker threads for each pool
        for pool_arc in self.shared_state.pools.iter() {
            for _ in 0..pool_arc.thread_count {
                let pool = pool_arc.clone();
                let shared = self.shared_state.clone();
                let thread = thread::spawn(move || {
                    println!("Starting thread");
                    while shared.should_run.load(Ordering::Relaxed) {
                        executor_cycle(pool.as_ref(), shared.as_ref());
                    }
                    println!("leaving exec cycle");
                });
                self.threads.push(thread);
            }
        }

        // Spawn the periodic trigger thread; it owns exec_times entirely so no mutex is needed
        let shared_state = self.shared_state.clone();
        let thread = thread::spawn(move || {
            let now = task::time::FrameworkTime::from_wall_clock();
            let mut exec_times: VecDeque<TimeTriggeredTask> = VecDeque::new();
            for (index, task) in shared_state.tasks.iter().enumerate() {
                if let Some(t) = task.lock().unwrap().get_next_requested_execution_time(now) {
                    exec_times.push_back(TimeTriggeredTask {
                        index,
                        requested_exec_time: t,
                    });
                }
            }
            while shared_state.should_run.load(Ordering::Relaxed) {
                periodic_trigger_thread(shared_state.as_ref(), &mut exec_times);
            }
        });
        self.threads.push(thread);
    }

    pub fn stop_threads(&mut self) -> Result<(), Vec<usize>> {
        self.shared_state.should_run.store(false, Ordering::Relaxed);

        // Send one shutdown sentinel per worker so every blocked recv() unblocks
        for pool in self.shared_state.pools.iter() {
            for _ in 0..pool.thread_count {
                let _ = pool.work_tx.send(SHUTDOWN_SENTINEL);
            }
        }

        // Wake the periodic trigger thread
        self.shared_state.periodic_cond_var.notify_all();

        println!("Joining threads...");
        let mut thread_join_result = vec![];
        for (thread_idx, t) in self.threads.drain(..).enumerate() {
            match t.join() {
                Ok(()) => {}
                Err(_) => {
                    thread_join_result.push(thread_idx);
                }
            }
            println!("joined thread");
        }
        println!("all threads joined");

        self.cleanup_buffers();

        if thread_join_result.is_empty() {
            return Ok(());
        }
        Err(thread_join_result)
    }

    fn cleanup_buffers(&mut self) {
        for arc_task in self.shared_state.tasks.iter() {
            let task = arc_task.lock().unwrap();
            for subscriber in task.get_subscribers().iter() {
                subscriber.cleanup_buffers();
            }
        }
    }
}

impl TaskEnqueuer for SharedThreadPoolState {
    fn enqueue_task(&self, task_index: usize) {
        self.trigger_task(task_index);
    }
}

/// A lightweight, cloneable handle that can signal the executor to stop.
/// Holds only the shared state Arc so it is safe to call from worker threads
/// without acquiring any lock on the LiveExecutor itself.
pub struct StopSignal(Arc<SharedThreadPoolState>);

impl ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        self.0.should_run.store(false, Ordering::Relaxed);
        for pool in self.0.pools.iter() {
            for _ in 0..pool.thread_count {
                let _ = pool.work_tx.send(SHUTDOWN_SENTINEL);
            }
        }
        self.0.periodic_cond_var.notify_all();
    }
}

impl Executor for LiveExecutor {
    fn start(&mut self) {
        self.start_threads();
    }

    fn stop(&mut self) -> Result<(), ExecutorError> {
        self.stop_threads().map_err(ExecutorError::PanickedThreads)
    }

    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal> {
        Arc::new(StopSignal(self.shared_state.clone()))
    }

    fn is_running(&self) -> bool {
        self.shared_state.should_run.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, OnceLock},
        thread::sleep,
        time,
    };

    use task::{
        callback::connect_callbacks,
        executor::{Executor, ThreadPoolConfig},
        test_tasks::*,
    };

    use crate::LiveExecutor;

    #[test]
    fn test_thread_pool_exec() {
        let string_store = StringCollector::make_string_store();
        let stop_signal_cell = Arc::new(OnceLock::new());

        let mut callbacks = vec![
            IncrementingIntegerPublisher::build_connected_callback(),
            FizzBuzzCalculator::build_connected_callback(),
            StringCollector::build_connected_callback(
                string_store.clone(),
                stop_signal_cell.clone(),
                1,
            ),
        ];
        let connect_result = connect_callbacks(&mut callbacks);
        assert!(
            connect_result.is_ok(),
            "Result was {}",
            connect_result.unwrap_err()
        );
        assert!(callbacks[0].get_publishers()[0].get_config().channel_name == "integer");
        assert!(callbacks[1].get_subscribers()[0].get_config().channel_name == "integer");

        let mut exec = LiveExecutor::new(1, callbacks);

        stop_signal_cell.set(exec.stop_signal()).ok();
        exec.start_threads();

        let deadline = time::Instant::now() + time::Duration::from_secs(10);
        while exec.is_running() && time::Instant::now() < deadline {
            sleep(time::Duration::from_millis(10));
        }
        assert!(
            !exec.is_running(),
            "Executor did not stop itself within 10 seconds"
        );

        let stop_result = exec.stop_threads();
        assert!(stop_result.is_ok());

        assert!(!string_store.lock().unwrap().is_empty());
    }

    #[test]
    fn test_multi_pool_exec() {
        let string_store = StringCollector::make_string_store();
        let stop_signal_cell = Arc::new(OnceLock::new());

        let mut all_callbacks = vec![
            IncrementingIntegerPublisher::build_connected_callback(),
            FizzBuzzCalculator::build_connected_callback(),
            StringCollector::build_connected_callback(
                string_store.clone(),
                stop_signal_cell.clone(),
                1,
            ),
        ];
        let connect_result = connect_callbacks(&mut all_callbacks);
        assert!(
            connect_result.is_ok(),
            "Result was {}",
            connect_result.unwrap_err()
        );

        let pool1 = vec![all_callbacks.remove(0)];
        let pool2 = all_callbacks;

        let mut exec = LiveExecutor::new_multi_pool(vec![
            ThreadPoolConfig::new(1, pool1),
            ThreadPoolConfig::new(1, pool2),
        ]);

        stop_signal_cell.set(exec.stop_signal()).ok();
        exec.start_threads();

        let deadline = time::Instant::now() + time::Duration::from_secs(10);
        while exec.is_running() && time::Instant::now() < deadline {
            sleep(time::Duration::from_millis(10));
        }
        assert!(
            !exec.is_running(),
            "Multi-pool executor did not stop itself within 10 seconds"
        );

        let stop_result = exec.stop_threads();
        assert!(stop_result.is_ok());

        assert!(!string_store.lock().unwrap().is_empty());
    }
}
