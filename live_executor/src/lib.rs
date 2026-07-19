use crossbeam::channel::{self, Receiver, Sender};
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use task::time::FrameworkTime;

use task::callback::CallbackNode;
use task::context::Context;
use task::executor::{CallbackNodeEnqueuer, Executor, ExecutorStopSignal, ThreadPoolConfig};

/// Sent into a pool's work channel to unblock workers on shutdown.
const SHUTDOWN_SENTINEL: usize = usize::MAX;

#[derive(Debug)]
pub struct LiveExecutorError {
    pub panicked_thread_indices: Vec<usize>,
}

impl std::fmt::Display for LiveExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "threads panicked: {:?}", self.panicked_thread_indices)
    }
}

impl std::error::Error for LiveExecutorError {}

#[derive(Clone, Copy)]
struct TimeTriggeredNode {
    index: usize,
    requested_exec_time: FrameworkTime,
}

struct PoolState {
    thread_count: usize,
    work_tx: Sender<usize>,
    work_rx: Receiver<usize>,
}

/// Everything callback nodes need to enqueue work. Owns no nodes, so nodes can hold
/// a strong Arc<EnqueueState> without creating a reference cycle.
struct EnqueueState {
    pools: Vec<Arc<PoolState>>,
    node_to_pool: Vec<usize>,
    /// One flag per node: true while the node index is sitting in the work channel.
    /// CAS'd to true on enqueue, cleared to false before execution begins, preventing
    /// duplicate entries in the channel.
    node_enqueued: Vec<AtomicBool>,
}

impl EnqueueState {
    fn trigger_node(&self, index: usize) {
        if self.node_enqueued[index]
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let pool = &self.pools[self.node_to_pool[index]];
            let _ = pool.work_tx.send(index);
        }
    }
}

impl CallbackNodeEnqueuer for EnqueueState {
    fn enqueue_node(&self, node_index: usize) {
        self.trigger_node(node_index);
    }
}

struct SharedThreadPoolState {
    enqueue_state: Arc<EnqueueState>,

    /// Mutex and condvar used only to sleep/wake the periodic trigger thread
    periodic_mutex: Mutex<()>,
    periodic_cond_var: Condvar,

    /// Storage of all callback nodes across all pools - each node has its own mutex
    /// for fine-grained locking, allowing concurrent execution across pools.
    nodes: Vec<Arc<Mutex<CallbackNode>>>,

    /// Whether the thread pool should continue running
    should_run: AtomicBool,
}

impl fmt::Display for SharedThreadPoolState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Should run: {}", self.should_run.load(Ordering::Relaxed))?;
        writeln!(f, "All callback nodes:")?;
        for (index, arc_node) in self.nodes.iter().enumerate() {
            let node = arc_node.lock().unwrap();
            writeln!(f, "\t ----------------------------------")?;
            writeln!(
                f,
                "\t Index:{}, Name: {}, Pool: {}",
                index,
                node.get_name(),
                self.enqueue_state.node_to_pool[index]
            )?;
            writeln!(f, "\t Able to run: {}", node.able_to_run())?;
            writeln!(
                f,
                "\t Subscribers request execution: {}",
                node.subscribers_request_execution()
            )?;
            writeln!(f, "\t Subscribers")?;
            for s in node.get_subscribers().iter() {
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
    exec_times: &mut VecDeque<TimeTriggeredNode>,
) {
    let now = task::time::FrameworkTime::from_wall_clock();

    let maybe_earliest = exec_times
        .iter()
        .min_by_key(|node| node.requested_exec_time)
        .copied();

    let time_triggered_node = match maybe_earliest {
        Some(t) => t,
        None => {
            let guard = shared_state.periodic_mutex.lock().unwrap();
            drop(shared_state.periodic_cond_var.wait(guard));
            return;
        }
    };

    if let Some(duration) = time_triggered_node
        .requested_exec_time
        .checked_duration_since(now)
    {
        // Wait until it's time to run the earliest node, or until woken early (e.g. shutdown)
        let guard = shared_state.periodic_mutex.lock().unwrap();
        let _ = shared_state
            .periodic_cond_var
            .wait_timeout(guard, duration)
            .unwrap();
    }

    // Re-sample the clock after waking: `wait_timeout` can return early (spurious
    // wakeup, shutdown notification) or late (OS scheduling slop), so the `now`
    // captured before waiting may no longer reflect reality. Comparing — and
    // rescheduling — against a fresh timestamp is essential: comparing against the
    // stale `now` can make the node look "not due yet" forever once it actually
    // becomes overdue, permanently livelocking its periodic trigger.
    let now = task::time::FrameworkTime::from_wall_clock();

    if now >= time_triggered_node.requested_exec_time {
        shared_state
            .enqueue_state
            .trigger_node(time_triggered_node.index);

        let node_guard = shared_state.nodes[time_triggered_node.index]
            .lock()
            .unwrap();

        let next_exec_time = node_guard
            .get_next_requested_execution_time(now)
            .unwrap_or(task::time::FrameworkTime::MAX);

        for node in exec_times.iter_mut() {
            if node.index == time_triggered_node.index {
                node.requested_exec_time = next_exec_time;
            }
        }
    }
}

pub(crate) fn process_work_item(index: usize, shared_state: &SharedThreadPoolState) {
    // Clear the enqueued flag before running so any triggers that arrive during
    // execution are captured, not dropped
    shared_state.enqueue_state.node_enqueued[index].store(false, Ordering::Release);

    let mut node_guard = shared_state.nodes[index].lock().unwrap();
    let ctx = Context::new(task::time::FrameworkTime::from_wall_clock());
    node_guard.drain_subscribers();
    let _ = node_guard.run(&ctx);
    node_guard.flush_publishers(ctx.now);
}

fn run_executor_thread(pool_state: &PoolState, shared_state: &SharedThreadPoolState) {
    loop {
        // Block waiting on work
        let index = match pool_state.work_rx.recv() {
            Ok(SHUTDOWN_SENTINEL) => break,
            Ok(idx) => idx,
            Err(_) => break, // channel disconnected
        };

        // should_run may have flipped during the blocking recv.
        if !shared_state.should_run.load(Ordering::Relaxed) {
            break;
        }

        process_work_item(index, shared_state);
    }
}

#[cfg(test)]
fn no_alloc_worker_loop(pool: Arc<PoolState>, shared: Arc<SharedThreadPoolState>) {
    loop {
        let index = match pool.work_rx.recv() {
            Ok(SHUTDOWN_SENTINEL) => break,
            Ok(idx) => idx,
            Err(_) => break,
        };
        if !shared.should_run.load(Ordering::Relaxed) {
            break;
        }
        assert_no_alloc::assert_no_alloc(|| process_work_item(index, &shared));
    }
}

impl LiveExecutor {
    /// Create a single shared thread pool with `num_threads` workers for all callback nodes.
    pub fn new(num_threads: usize, nodes: Vec<CallbackNode>) -> Self {
        Self::new_multi_pool(vec![ThreadPoolConfig::new(num_threads, nodes)])
    }

    /// Create multiple independent thread pools in one executor.
    ///
    /// Callback nodes in different pools execute on separate worker threads, enabling
    /// priority separation: put latency-sensitive nodes in a pool with dedicated
    /// threads and background nodes in another. The periodic trigger thread and
    /// node storage are shared across all pools to minimise overhead.
    pub fn new_multi_pool(pools: Vec<ThreadPoolConfig>) -> Self {
        let mut all_arc_nodes: Vec<Arc<Mutex<CallbackNode>>> = Vec::new();
        let mut node_to_pool: Vec<usize> = Vec::new();
        let mut pool_states: Vec<Arc<PoolState>> = Vec::new();

        for (pool_idx, pool) in pools.into_iter().enumerate() {
            // Channel capacity: one slot per node in this pool (AtomicBool dedup ensures
            // at most one entry per node) plus one per worker thread for shutdown sentinels
            let capacity = pool.nodes.len() + pool.thread_count;
            let (work_tx, work_rx) = channel::bounded(capacity.max(1));

            pool_states.push(Arc::new(PoolState {
                thread_count: pool.thread_count,
                work_tx,
                work_rx,
            }));

            for node in pool.nodes {
                node_to_pool.push(pool_idx);
                all_arc_nodes.push(Arc::new(Mutex::new(node)));
            }
        }

        let num_nodes = all_arc_nodes.len();
        let enqueue_state = Arc::new(EnqueueState {
            pools: pool_states,
            node_to_pool,
            node_enqueued: (0..num_nodes).map(|_| AtomicBool::new(false)).collect(),
        });
        let shared_state = Arc::new(SharedThreadPoolState {
            enqueue_state: enqueue_state.clone(),
            periodic_mutex: Mutex::new(()),
            periodic_cond_var: Condvar::new(),
            nodes: all_arc_nodes,
            should_run: true.into(),
        });

        let enqueuer = enqueue_state as Arc<dyn CallbackNodeEnqueuer>;
        for (index, arc_node) in shared_state.nodes.iter().enumerate() {
            arc_node
                .lock()
                .unwrap()
                .register_with_executor(index, enqueuer.clone());
        }

        LiveExecutor {
            threads: Vec::new(),
            shared_state,
        }
    }

    fn start_threads_with(
        &mut self,
        spawn_worker: impl Fn(Arc<PoolState>, Arc<SharedThreadPoolState>) -> thread::JoinHandle<()>,
    ) {
        for index in 0..self.shared_state.nodes.len() {
            let node = self.shared_state.nodes[index].lock().unwrap();
            if node.subscribers_request_execution() && node.able_to_run() {
                drop(node);
                self.shared_state.enqueue_state.trigger_node(index);
            }
        }

        for pool_arc in self.shared_state.enqueue_state.pools.iter() {
            for _ in 0..pool_arc.thread_count {
                let pool = pool_arc.clone();
                let shared = self.shared_state.clone();
                self.threads.push(spawn_worker(pool, shared));
            }
        }

        let shared_state = self.shared_state.clone();
        let thread = thread::spawn(move || {
            let now = task::time::FrameworkTime::from_wall_clock();
            let mut exec_times: VecDeque<TimeTriggeredNode> = VecDeque::new();
            for (index, node) in shared_state.nodes.iter().enumerate() {
                if let Some(t) = node.lock().unwrap().get_next_requested_execution_time(now) {
                    exec_times.push_back(TimeTriggeredNode {
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

    pub fn start_threads(&mut self) {
        self.start_threads_with(|pool, shared| {
            thread::spawn(move || {
                println!("Starting thread");
                run_executor_thread(pool.as_ref(), shared.as_ref());
                println!("leaving exec cycle");
            })
        })
    }

    pub fn stop_threads(&mut self) -> Result<(), Vec<usize>> {
        self.shared_state.should_run.store(false, Ordering::Relaxed);

        // Send one shutdown sentinel per worker so every blocked recv() unblocks.
        // try_send: if the bounded channel is full, a sentinel (or a real node the
        // worker will drain then exit on) is already in flight — skip rather than
        // block, since stop_threads must never hang waiting on a full queue.
        for pool in self.shared_state.enqueue_state.pools.iter() {
            for _ in 0..pool.thread_count {
                let _ = pool.work_tx.try_send(SHUTDOWN_SENTINEL);
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
        for arc_node in self.shared_state.nodes.iter() {
            let node = arc_node.lock().unwrap();
            for subscriber in node.get_subscribers().iter() {
                subscriber.cleanup_buffers();
            }
        }
    }
}

/// A lightweight, cloneable handle that can signal the executor to stop.
/// Holds a Weak<SharedThreadPoolState> so callback nodes can own a StopSignal without
/// creating a reference cycle back through the executor's node list.
pub struct StopSignal(Weak<SharedThreadPoolState>);

impl ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        let Some(state) = self.0.upgrade() else {
            return;
        };
        // Just signal the intent to stop. Sentinel injection is `stop_threads`'
        // job — having both paths push sentinels can overflow a bounded pool
        // channel. The caller is expected to invoke `stop_threads` to unblock
        // and join the workers.
        state.should_run.store(false, Ordering::Relaxed);
        state.periodic_cond_var.notify_all();
    }
}

impl Executor for LiveExecutor {
    type Error = LiveExecutorError;

    fn start(&mut self) {
        self.start_threads();
    }

    fn stop(&mut self) -> Result<(), LiveExecutorError> {
        self.stop_threads()
            .map_err(|panicked_thread_indices| LiveExecutorError {
                panicked_thread_indices,
            })
    }

    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal> {
        Arc::new(StopSignal(Arc::downgrade(&self.shared_state)))
    }

    fn is_running(&self) -> bool {
        self.shared_state.should_run.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
impl LiveExecutor {
    fn start_threads_no_alloc(&mut self) {
        self.start_threads_with(|pool, shared| {
            thread::spawn(move || no_alloc_worker_loop(pool, shared))
        })
    }
}

#[cfg(test)]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, OnceLock,
            atomic::{AtomicUsize, Ordering},
        },
        thread::sleep,
        time,
    };

    use task::{
        callback::{Callback, CallbackNode, InputKind, OutputKind, Run, connect_callback_nodes},
        callback_builder::CallbackBuilder,
        context::Context,
        executor::{Executor, ExecutorStopSignal, ThreadPoolConfig},
        generic_publisher::GenericPublisher,
        generic_subscriber::GenericSubscriber,
        input::RequiredInput,
        output::Output,
        publisher::Publisher,
        subscriber::Subscriber,
    };
    use test_tasks::*;

    use crate::LiveExecutor;

    /// A callback with no inputs that runs purely off its periodic trigger,
    /// counting how many times it has run and requesting a stop once it
    /// reaches `target_runs`. Used to exercise sustained periodic scheduling.
    struct PeriodicCounter {
        run_count: Arc<AtomicUsize>,
        target_runs: usize,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    }

    impl Callback for PeriodicCounter {
        fn run_generic(
            &mut self,
            _subscribers: &mut [Box<dyn GenericSubscriber>],
            _publishers: &mut [Box<dyn GenericPublisher>],
            _ctx: &Context,
        ) -> Run {
            let run_number = self.run_count.fetch_add(1, Ordering::SeqCst) + 1;
            if run_number >= self.target_runs {
                if let Some(signal) = self.stop_signal.get() {
                    signal.request_stop();
                }
            }
            Run::new(1)
        }

        fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
            vec![]
        }

        fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
            vec![]
        }
    }

    struct NoAllocPublisher {
        value: u64,
    }

    impl Callback for NoAllocPublisher {
        fn run_generic(
            &mut self,
            _subscribers: &mut [Box<dyn GenericSubscriber>],
            publishers: &mut [Box<dyn GenericPublisher>],
            _ctx: &Context,
        ) -> Run {
            let mut output = Output::<u64>::new_downcasted(&mut *publishers[0]);
            *output = self.value;
            self.value = self.value.wrapping_add(1);
            output.send();
            Run::new(1)
        }

        fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
            vec![]
        }

        fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
            vec![Box::new(Publisher::<u64>::new(OutputKind::Default.into()))]
        }
    }

    struct NoAllocSubscriber {
        messages_received: Arc<AtomicUsize>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target_count: usize,
    }

    impl Callback for NoAllocSubscriber {
        fn run_generic(
            &mut self,
            subscribers: &mut [Box<dyn GenericSubscriber>],
            _publishers: &mut [Box<dyn GenericPublisher>],
            _ctx: &Context,
        ) -> Run {
            let _input = RequiredInput::<u64>::new_downcasted(&mut *subscribers[0]);
            let count = self.messages_received.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= self.target_count {
                if let Some(signal) = self.stop_signal.get() {
                    signal.request_stop();
                }
            }
            Run::new(1)
        }

        fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
            vec![Box::new(Subscriber::<u64>::new(InputKind::Required.into()))]
        }

        fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
            vec![]
        }
    }

    #[test]
    fn test_thread_pool_exec() {
        let string_store = StringCollector::make_string_store();
        let stop_signal_cell = Arc::new(OnceLock::new());

        let mut nodes = vec![
            IncrementingIntegerPublisher::build_callback_node(),
            FizzBuzzCalculator::build_callback_node(),
            StringCollector::build_callback_node(string_store.clone(), stop_signal_cell.clone(), 1),
        ];
        let connect_result = connect_callback_nodes(&mut nodes);
        assert!(
            connect_result.is_ok(),
            "Result was {}",
            connect_result.unwrap_err()
        );
        assert!(nodes[0].get_publishers()[0].get_config().channel_name == "integer");
        assert!(nodes[1].get_subscribers()[0].get_config().channel_name == "integer");

        let mut exec = LiveExecutor::new(1, nodes);

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

        let mut all_nodes = vec![
            IncrementingIntegerPublisher::build_callback_node(),
            FizzBuzzCalculator::build_callback_node(),
            StringCollector::build_callback_node(string_store.clone(), stop_signal_cell.clone(), 1),
        ];
        let connect_result = connect_callback_nodes(&mut all_nodes);
        assert!(
            connect_result.is_ok(),
            "Result was {}",
            connect_result.unwrap_err()
        );

        let pool1 = vec![all_nodes.remove(0)];
        let pool2 = all_nodes;

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

    /// Regression test for a livelock in `periodic_trigger_thread`: a callback node driven
    /// purely by its periodic schedule (no subscribers to trigger it) must keep
    /// being re-triggered indefinitely, not just once or twice.
    #[test]
    fn test_sustained_periodic_trigger() {
        const TARGET_RUNS: usize = 50;

        #[cfg(not(miri))]
        const DEADLINE_SECS: u64 = 10;
        #[cfg(miri)]
        const DEADLINE_SECS: u64 = 120;

        let run_count = Arc::new(AtomicUsize::new(0));
        let stop_signal_cell = Arc::new(OnceLock::new());

        let callback: Box<dyn Callback> = Box::new(PeriodicCounter {
            run_count: run_count.clone(),
            target_runs: TARGET_RUNS,
            stop_signal: stop_signal_cell.clone(),
        });
        let subscribers = callback.build_subscribers();
        let publishers = callback.build_publishers();
        let mut connected =
            CallbackNode::new_with(callback, subscribers, publishers, "PeriodicCounter".into());
        connected.set_execution_time_callback(Box::new(|now| {
            Some(now + time::Duration::from_millis(2))
        }));
        connected.set_execution_duration_callback(Box::new(|| time::Duration::ZERO));

        let mut exec = LiveExecutor::new(1, vec![connected]);

        stop_signal_cell.set(exec.stop_signal()).ok();
        exec.start_threads();

        let deadline = time::Instant::now() + time::Duration::from_secs(DEADLINE_SECS);
        while exec.is_running() && time::Instant::now() < deadline {
            sleep(time::Duration::from_millis(10));
        }
        assert!(
            !exec.is_running(),
            "Executor did not reach {TARGET_RUNS} periodic runs within {DEADLINE_SECS} seconds (stuck at {})",
            run_count.load(Ordering::SeqCst)
        );

        let stop_result = exec.stop_threads();
        assert!(stop_result.is_ok());

        assert!(run_count.load(Ordering::SeqCst) >= TARGET_RUNS);
    }

    #[test]
    // I think miri makes its own global allocator shim based on https://github.com/rust-lang/miri/issues/1207
    // so we shouldn't conflict with it via the assert_no_alloc crate.
    #[cfg_attr(miri, ignore)]
    fn test_executor_worker_no_alloc() {
        println!("warming stdio buffers");

        const TARGET_COUNT: usize = 50;

        #[cfg(not(miri))]
        const DEADLINE_SECS: u64 = 10;
        #[cfg(miri)]
        const DEADLINE_SECS: u64 = 120;

        let messages_received = Arc::new(AtomicUsize::new(0));
        let stop_signal_cell = Arc::new(OnceLock::new());

        let publisher_node = CallbackBuilder::new(
            "NoAllocPublisher".into(),
            Box::new(NoAllocPublisher { value: 0 }),
        )
        .with_publisher_channels(&["no_alloc_integer"])
        .with_next_execution_time_callback(|now| Some(now + time::Duration::from_millis(2)))
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .build()
        .unwrap();

        let subscriber_node = CallbackBuilder::new(
            "NoAllocSubscriber".into(),
            Box::new(NoAllocSubscriber {
                messages_received: messages_received.clone(),
                stop_signal: stop_signal_cell.clone(),
                target_count: TARGET_COUNT,
            }),
        )
        .with_subscriber_channels(&["no_alloc_integer"])
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .build()
        .unwrap();

        let mut nodes = vec![publisher_node, subscriber_node];
        connect_callback_nodes(&mut nodes).expect("failed to connect callback nodes");

        let mut exec = LiveExecutor::new(1, nodes);

        stop_signal_cell.set(exec.stop_signal()).ok();
        exec.start_threads_no_alloc();

        let deadline = time::Instant::now() + time::Duration::from_secs(DEADLINE_SECS);
        while exec.is_running() && time::Instant::now() < deadline {
            sleep(time::Duration::from_millis(10));
        }
        assert!(
            !exec.is_running(),
            "Executor did not reach {TARGET_COUNT} messages within {DEADLINE_SECS} seconds (stuck at {})",
            messages_received.load(Ordering::SeqCst)
        );

        let stop_result = exec.stop_threads();
        assert!(stop_result.is_ok());

        assert!(messages_received.load(Ordering::SeqCst) >= TARGET_COUNT);
    }
}
