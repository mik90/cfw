use crossbeam::channel;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use task::callback::CallbackNode;
use task::callback_storage::{CallbackStorage, SharedCallbackNode, WorkerNodes};
use task::context::Context;
use task::execution_log::{self, ExecutionLogLevel, ExecutionLogMessage};
use task::executor::{
    CallbackNodeEnqueuer, Executor, ExecutorStopSignal, ThreadPoolConfig, TimeSource, WallClock,
};
use task::publisher::Publisher;
use task::time::FrameworkTime;

use crate::error::LiveExecutorError;
use crate::periodic::periodic_trigger_thread;
use crate::pool_state::{EnqueueState, PoolState, SharedThreadPoolState, TimeTriggeredNode};
use crate::stop_signal::StopSignal;
use crate::worker_logger::{WorkerLogger, WorkerLoggerInit};

const DEFAULT_LOG_FLUSH_PERIOD: Duration = Duration::from_millis(500);
const SHUTDOWN_SENTINEL: usize = usize::MAX;

pub struct LiveExecutor<T: TimeSource = WallClock> {
    worker_threads: Vec<thread::JoinHandle<()>>,
    periodic_thread: Option<thread::JoinHandle<()>>,
    shared_state: Arc<SharedThreadPoolState>,
    /// Authoritative callback-node storage, accessed only from the main
    /// thread. Worker threads hold their own `clone_shared()` clones.
    nodes: CallbackStorage,
    time_source: Arc<T>,
    log_publishers: Vec<Publisher<ExecutionLogMessage>>,
    per_pool_scratch_cap: Vec<usize>,
    flush_period: Duration,
}

impl<T: TimeSource + 'static> LiveExecutor<T> {
    fn new_multi_pool_core(pools: Vec<ThreadPoolConfig>, time_source: T) -> Self {
        let mut all_shared_nodes: Vec<Arc<SharedCallbackNode>> = Vec::new();
        let mut node_to_pool: Vec<usize> = Vec::new();
        let mut pool_states: Vec<Arc<PoolState>> = Vec::new();

        for (pool_idx, pool) in pools.into_iter().enumerate() {
            let capacity = pool.nodes.len() + pool.thread_count;
            let (work_tx, work_rx) = channel::bounded(capacity.max(1));

            pool_states.push(Arc::new(PoolState {
                thread_count: pool.thread_count,
                work_tx,
                work_rx,
            }));

            for node in pool.nodes.into_nodes() {
                node_to_pool.push(pool_idx);
                all_shared_nodes.push(node);
            }
        }

        let worker_count: usize = pool_states.iter().map(|p| p.thread_count).sum();

        let enqueue_state = Arc::new(EnqueueState {
            pools: pool_states,
            node_to_pool,
            nodes: all_shared_nodes.clone(),
        });
        let shared_state = Arc::new(SharedThreadPoolState {
            enqueue_state: enqueue_state.clone(),
            periodic_mutex: Mutex::new(()),
            periodic_cond_var: Condvar::new(),
            should_run: true.into(),
            worker_count,
            worker_liveness: (0..worker_count).map(|_| Mutex::new(())).collect(),
            barrier_count: AtomicUsize::new(0),
            cleanup_done: AtomicBool::new(false),
            shutdown_mutex: Mutex::new(()),
            shutdown_cv: Condvar::new(),
        });

        let nodes = CallbackStorage::from_shared(all_shared_nodes);

        let enqueuer: Arc<dyn CallbackNodeEnqueuer> = enqueue_state.clone();
        let now = time_source.now();
        for (index, node) in nodes.iter_shared().enumerate() {
            // Worker-style execution instead of `access`: registering
            // can synchronously enqueue a node whose inputs are already
            // ready (e.g. no required inputs), which flips the run state to
            // "triggered while running" — handled here by sending the index
            // to its pool, exactly like a worker releasing a re-run.
            let (_, reenqueue) = node.execute(now, |node| {
                node.register_with_executor(index, enqueuer.clone())
            });
            if reenqueue {
                enqueue_state.resend_node(index);
            }
        }

        LiveExecutor {
            worker_threads: Vec::new(),
            periodic_thread: None,
            shared_state,
            nodes,
            time_source: Arc::new(time_source),
            log_publishers: vec![],
            per_pool_scratch_cap: Vec::new(),
            flush_period: DEFAULT_LOG_FLUSH_PERIOD,
        }
    }

    pub fn new_multi_pool_with_time(pools: Vec<ThreadPoolConfig>, time_source: T) -> Self {
        Self::new_multi_pool_core(pools, time_source)
    }

    pub fn new_multi_pool_with_execution_log_and_time(
        pools: Vec<ThreadPoolConfig>,
        log_publishers: Vec<Publisher<ExecutionLogMessage>>,
        flush_period: Duration,
        time_source: T,
    ) -> Self {
        if !log_publishers.is_empty() {
            debug_assert_eq!(
                log_publishers.len(),
                pools.iter().map(|p| p.thread_count).sum::<usize>(),
                "execution-log publisher count must equal the total worker thread count"
            );
        }
        let per_pool_scratch_cap: Vec<usize> = pools
            .iter()
            .map(|pool| {
                pool.nodes
                    .iter_shared()
                    .filter_map(|node| {
                        node.try_access(|n| execution_log::worst_case_received_count(n))
                    })
                    .max()
                    .unwrap_or(0)
            })
            .collect();

        let mut exec = Self::new_multi_pool_core(pools, time_source);
        exec.log_publishers = log_publishers;
        exec.per_pool_scratch_cap = per_pool_scratch_cap;
        exec.flush_period = flush_period;
        exec
    }
}

impl LiveExecutor<WallClock> {
    pub fn new(num_threads: usize, nodes: Vec<CallbackNode>) -> Self {
        Self::new_multi_pool(vec![ThreadPoolConfig::new(num_threads, nodes)])
    }

    pub fn new_multi_pool(pools: Vec<ThreadPoolConfig>) -> Self {
        Self::new_multi_pool_core(pools, WallClock)
    }

    pub fn new_multi_pool_with_execution_log(
        pools: Vec<ThreadPoolConfig>,
        log_publishers: Vec<Publisher<ExecutionLogMessage>>,
        flush_period: Duration,
    ) -> Self {
        Self::new_multi_pool_with_execution_log_and_time(
            pools,
            log_publishers,
            flush_period,
            WallClock,
        )
    }
}

impl<T: TimeSource + 'static> LiveExecutor<T> {
    fn start_threads_with(
        &mut self,
        spawn_worker: impl Fn(
            Arc<PoolState>,
            Arc<SharedThreadPoolState>,
            Option<WorkerLoggerInit>,
            Arc<T>,
            usize,
            String,
            WorkerNodes,
        ) -> thread::JoinHandle<()>,
    ) -> Vec<thread::JoinHandle<()>> {
        self.shared_state.barrier_count.store(0, Ordering::Release);
        self.shared_state
            .cleanup_done
            .store(false, Ordering::Release);

        let now = self.time_source.now();
        for (index, node) in self.nodes.iter_shared().enumerate() {
            // Worker-style execute: seeds the periodic-scheduling snapshot
            // (so the periodic thread can plan purely from atomics) while
            // checking whether the node already has everything it needs.
            let (ready, _) = node.execute(now, |node| {
                node.subscribers_request_execution() && node.able_to_run()
            });
            if ready {
                self.shared_state.enqueue_state.trigger_node(index);
            }
        }

        let has_log_publishers = !self.log_publishers.is_empty();
        let mut log_publisher_drainer = self.log_publishers.drain(..);
        let mut handles = Vec::new();
        let mut worker_index = 0;

        for (pool_idx, pool_arc) in self.shared_state.enqueue_state.pools.iter().enumerate() {
            for thread_idx in 0..pool_arc.thread_count {
                let pool = pool_arc.clone();
                let shared = self.shared_state.clone();
                let ts = self.time_source.clone();
                let init = match has_log_publishers {
                    true => {
                        let publisher = log_publisher_drainer
                            .next()
                            .expect("Expected one publisher per thread");
                        Some(WorkerLoggerInit {
                            publisher,
                            flush_period: self.flush_period,
                            scratch_capacity: self.per_pool_scratch_cap[pool_idx],
                        })
                    }
                    false => None,
                };
                let thread_name = format!("cfw_pool_{pool_idx}_t_{thread_idx}");
                // Each worker gets its own vec of shared node handles; nodes
                // are only ever accessed through these per-thread clones.
                let worker_nodes = self.nodes.clone_shared();
                handles.push(spawn_worker(
                    pool,
                    shared,
                    init,
                    ts,
                    worker_index,
                    thread_name,
                    worker_nodes,
                ));
                worker_index += 1;
            }
        }

        handles
    }

    pub fn start_threads(&mut self) {
        let time_source = self.time_source.clone();
        self.worker_threads =
            self.start_threads_with(move |pool, shared, init, _ts, worker_index, name, nodes| {
                let ts = time_source.clone();
                thread::Builder::new()
                    .name(name)
                    .spawn(move || {
                        println!("Starting thread");
                        let logger = init.map(|i| WorkerLogger::new(i, ts.now()));
                        run_executor_thread(
                            pool.as_ref(),
                            shared.as_ref(),
                            &nodes,
                            logger,
                            ts.as_ref(),
                            worker_index,
                        )
                    })
                    .expect("spawn worker thread")
            });

        self.spawn_periodic_thread_with(|shared_state, nodes, exec_times, time_source| {
            periodic_trigger_thread(shared_state, nodes, exec_times, time_source);
        });
    }

    fn spawn_periodic_thread_with<F>(&mut self, mut body: F)
    where
        F: FnMut(
                &SharedThreadPoolState,
                &[Arc<SharedCallbackNode>],
                &mut VecDeque<TimeTriggeredNode>,
                &T,
            ) + Send
            + 'static,
    {
        let shared_state = self.shared_state.clone();
        let time_source = self.time_source.clone();
        let worker_nodes = self.nodes.clone_shared();
        let nodes: Vec<Arc<SharedCallbackNode>> = worker_nodes.iter().cloned().collect();
        self.periodic_thread = Some(
            thread::Builder::new()
                .name(String::from("cfw_periodic"))
                .spawn(move || {
                    let mut exec_times: VecDeque<TimeTriggeredNode> = VecDeque::new();
                    // Plan purely from the snapshots seeded by the executor
                    // before spawning: the periodic thread never reads node
                    // internals. (A node already running at startup has a
                    // snapshot from its seeding too.)
                    for (index, node) in nodes.iter().enumerate() {
                        if let Some(t) = node.next_exec_time() {
                            exec_times.push_back(TimeTriggeredNode {
                                index,
                                requested_exec_time: t,
                            });
                        }
                    }
                    while shared_state.should_run.load(Ordering::Relaxed) {
                        body(
                            shared_state.as_ref(),
                            &nodes,
                            &mut exec_times,
                            time_source.as_ref(),
                        );
                    }
                })
                .expect("spawn periodic thread"),
        );
    }

    pub fn stop_threads(&mut self) -> Result<(), Vec<usize>> {
        self.shared_state.should_run.store(false, Ordering::Relaxed);

        for pool in self.shared_state.enqueue_state.pools.iter() {
            for _ in 0..pool.thread_count {
                let _ = pool.work_tx.try_send(SHUTDOWN_SENTINEL);
            }
        }

        self.shared_state.periodic_cond_var.notify_all();

        // Wait for every worker to finish — either at the barrier (clean) or
        // fully exited (panicked — `is_finished` true, no barrier entry).
        {
            use std::sync::atomic::Ordering as O;
            let guard = self.shared_state.shutdown_mutex.lock().unwrap();
            // drop: discard the MutexGuard from wait_while immediately, releasing
            // shutdown_mutex. _ = ... would trigger let_underscore_lock.
            drop(self.shared_state.shutdown_cv.wait_while(guard, |_| {
                let at_barrier = self.shared_state.barrier_count.load(O::Acquire);
                let finished = self
                    .worker_threads
                    .iter()
                    .filter(|h| h.is_finished())
                    .count();
                at_barrier + finished < self.shared_state.worker_count
            }));
        }

        // Check if any worker panicked via mutex poisoning.
        let any_panicked = self
            .shared_state
            .worker_liveness
            .iter()
            .any(|m| m.is_poisoned());

        if any_panicked {
            // A panicked worker freed its publisher's arena during unwind.
            // Skip cleanup_buffers to avoid use-after-free.
            // The executor is shutting down in an error state; caller gets Err.
        } else {
            // All workers are parked at the barrier with publishers alive.
            self.nodes.cleanup_subscribers();
        }

        // Release workers from the barrier
        self.shared_state
            .cleanup_done
            .store(true, Ordering::Release);
        self.shared_state.shutdown_cv.notify_all();

        // Join worker threads
        let mut panicked_indices = vec![];
        for (i, handle) in self.worker_threads.drain(..).enumerate() {
            match handle.join() {
                Ok(()) => {}
                Err(_) => panicked_indices.push(i),
            }
        }

        // Join periodic thread
        if let Some(handle) = self.periodic_thread.take() {
            let _ = handle.join();
        }

        if panicked_indices.is_empty() {
            Ok(())
        } else {
            Err(panicked_indices)
        }
    }
}

fn process_work_item(
    index: usize,
    nodes: &[Arc<SharedCallbackNode>],
    shared_state: &SharedThreadPoolState,
    logger: Option<&mut WorkerLogger>,
    now: FrameworkTime,
) {
    let ctx = Context::new(now);

    // Worker-style execution: claim the node, run the work, refresh the
    // periodic snapshot while still holding it, then release. `execute`
    // releases the node before returning, so the re-send below can only ever
    // run once the node is free (and, if a trigger arrived mid-run, already
    // back in `Enqueued`).
    let (_, reenqueue) = nodes[index].execute(now, |node_guard| {
        match logger {
            Some(logger) => {
                if !logger.has_data() {
                    // Just track drop and continue
                    node_guard.drain_subscribers();
                    let _ = node_guard.run(&ctx);
                    node_guard.flush_publishers(ctx.now);
                    return;
                }

                match node_guard.execution_log_level() {
                    ExecutionLogLevel::Off => {
                        node_guard.drain_subscribers();
                        let _ = node_guard.run(&ctx);
                        node_guard.flush_publishers(ctx.now);
                    }
                    ExecutionLogLevel::Duration => {
                        node_guard.drain_subscribers();
                        let start = task::time::FrameworkTime::from_wall_clock();
                        let _ = node_guard.run(&ctx);
                        let end = task::time::FrameworkTime::from_wall_clock();
                        let duration = end.checked_duration_since(start).unwrap_or(Duration::ZERO);

                        logger.record_duration_only(index as u32, ctx.now, duration);

                        node_guard.flush_publishers(ctx.now);

                        logger.maybe_flush_period(ctx.now);
                    }
                    ExecutionLogLevel::Whole => {
                        node_guard.drain_subscribers();
                        logger.recv_scratch_clear();
                        let mut ordinal = 0u16;
                        node_guard.callback().for_each_subscriber(&mut |sub| {
                            let ordinal_val = ordinal;
                            ordinal += 1;
                            sub.for_each_queued_input(&mut |header, _payload| {
                                logger.recv_push(task::execution_log::LoggedMessage {
                                    ordinal: ordinal_val,
                                    direction: task::execution_log::Direction::Received,
                                    header: *header,
                                });
                            });
                        });
                        let start = task::time::FrameworkTime::from_wall_clock();
                        let _ = node_guard.run(&ctx);
                        let end = task::time::FrameworkTime::from_wall_clock();
                        let duration = end.checked_duration_since(start).unwrap_or(Duration::ZERO);

                        logger.begin_execution(index as u32, ctx.now, duration);

                        logger.drain_recv_into_current();

                        node_guard.flush_publishers_logged(ctx.now, &mut |ordinal, header| {
                            logger.append(task::execution_log::LoggedMessage {
                                ordinal: ordinal as u16,
                                direction: task::execution_log::Direction::Published,
                                header: *header,
                            });
                        });

                        logger.maybe_flush_period(ctx.now);
                    }
                }
            }
            None => {
                node_guard.drain_subscribers();
                let _ = node_guard.run(&ctx);
                node_guard.flush_publishers(ctx.now);
            }
        }
    });

    // `execute` already released the node; only now that it is free (and, on
    // a mid-run trigger, already `Enqueued`) feed the index back to the pool's
    // channel so a free worker can claim it immediately.
    if reenqueue {
        shared_state.enqueue_state.resend_node(index);
    }
}

fn worker_loop_core<T: TimeSource>(
    pool_state: &PoolState,
    shared_state: &SharedThreadPoolState,
    nodes: &[Arc<SharedCallbackNode>],
    mut logger: Option<WorkerLogger>,
    time_source: &T,
    worker_index: usize,
    mut process: impl FnMut(
        usize,
        &[Arc<SharedCallbackNode>],
        &SharedThreadPoolState,
        Option<&mut WorkerLogger>,
        FrameworkTime,
    ),
) {
    let _alive = shared_state.worker_liveness[worker_index].lock().unwrap();

    loop {
        let index = match pool_state.work_rx.recv() {
            Ok(SHUTDOWN_SENTINEL) => break,
            Ok(idx) => idx,
            Err(_) => break,
        };

        if !shared_state.should_run.load(Ordering::Relaxed) {
            break;
        }

        process(
            index,
            nodes,
            shared_state,
            logger.as_mut(),
            time_source.now(),
        );
    }

    // Flush remaining entries into the channel BEFORE reaching the barrier,
    // so they're captured by the main thread's cleanup_buffers.
    if let Some(logger) = logger.as_mut() {
        logger.flush_remaining(time_source.now());
    }

    // Update the predicate while holding the same mutex used by the waiter.
    // Otherwise the notification can be lost between the waiter's predicate
    // check and its call to `wait`.
    let guard = shared_state.shutdown_mutex.lock().unwrap();
    if shared_state.barrier_count.fetch_add(1, Ordering::AcqRel) + 1 == shared_state.worker_count {
        shared_state.shutdown_cv.notify_all();
    }

    // drop: discard the MutexGuard from wait_while immediately, releasing
    // shutdown_mutex. _ = ... would trigger let_underscore_lock.
    drop(shared_state.shutdown_cv.wait_while(guard, |_| {
        !shared_state.cleanup_done.load(Ordering::Acquire)
    }));
}

fn run_executor_thread<T: TimeSource>(
    pool_state: &PoolState,
    shared_state: &SharedThreadPoolState,
    nodes: &[Arc<SharedCallbackNode>],
    logger: Option<WorkerLogger>,
    time_source: &T,
    worker_index: usize,
) {
    worker_loop_core(
        pool_state,
        shared_state,
        nodes,
        logger,
        time_source,
        worker_index,
        process_work_item,
    );
}

#[cfg(test)]
fn no_alloc_worker_loop<T: TimeSource>(
    pool_state: &PoolState,
    shared_state: &SharedThreadPoolState,
    nodes: &[Arc<SharedCallbackNode>],
    logger: Option<WorkerLogger>,
    time_source: &T,
    worker_index: usize,
) {
    worker_loop_core(
        pool_state,
        shared_state,
        nodes,
        logger,
        time_source,
        worker_index,
        |index, nodes, shared_state, logger, now| {
            assert_no_alloc::assert_no_alloc(|| {
                process_work_item(index, nodes, shared_state, logger, now)
            })
        },
    );
}

impl<T: TimeSource + 'static> Executor for LiveExecutor<T> {
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
impl LiveExecutor<WallClock> {
    fn start_threads_no_alloc(&mut self) {
        let time_source = self.time_source.clone();
        self.worker_threads =
            self.start_threads_with(move |pool, shared, init, _ts, worker_index, name, nodes| {
                let pool = pool.clone();
                let shared = shared.clone();
                let ts = time_source.clone();
                thread::Builder::new()
                    .name(name)
                    .spawn(move || {
                        let logger = init.map(|i| WorkerLogger::new(i, ts.now()));
                        no_alloc_worker_loop(
                            pool.as_ref(),
                            shared.as_ref(),
                            &nodes,
                            logger,
                            ts.as_ref(),
                            worker_index,
                        )
                    })
                    .expect("spawn worker thread")
            });

        self.spawn_periodic_thread_with(|shared_state, nodes, exec_times, time_source| {
            assert_no_alloc::assert_no_alloc(|| {
                periodic_trigger_thread(shared_state, nodes, exec_times, time_source);
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicUsize, Ordering},
        },
        thread::sleep,
        time,
    };

    use task::{
        callback::{
            Callback, CallbackNode, CallbackViews, InputKind, OutputKind, PortMut, Run,
            connect_callback_nodes,
        },
        callback_builder::CallbackBuilder,
        context::Context,
        execution_log::ExecutionLogLevel,
        executor::{Executor, ExecutorStopSignal, ThreadPoolConfig},
        generic_publisher::GenericPublisher,
        generic_subscriber::GenericSubscriber,
        input::{OptionalInput, RequiredInput},
        output::Output,
        publisher::Publisher,
        subscriber::{Subscriber, SubscriberConfig},
    };
    use test_tasks::*;

    use super::LiveExecutor;

    struct NoAllocPublisher {
        publisher: Publisher<u64>,
        value: u64,
    }

    impl Callback for NoAllocPublisher {
        fn run(&mut self, _ctx: &Context) -> Run {
            let mut output = Output::<u64>::new_default(&mut self.publisher);
            *output = self.value;
            self.value = self.value.wrapping_add(1);
            output.send();
            Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
        fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
            f(&self.publisher);
        }
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
            f(&mut self.publisher);
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            f(PortMut::Publisher(&mut self.publisher));
        }
    }

    struct OptionalTriggerSubscriber {
        subscriber: Subscriber<u64>,
        messages_received: Arc<AtomicUsize>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target_count: usize,
    }

    impl Callback for OptionalTriggerSubscriber {
        fn run(&mut self, _ctx: &Context) -> Run {
            let mut input = OptionalInput::<u64>::new(&self.subscriber);
            while input.value().is_some() {
                let count = self.messages_received.fetch_add(1, Ordering::SeqCst) + 1;
                if count >= self.target_count
                    && let Some(signal) = self.stop_signal.get()
                {
                    signal.request_stop();
                }
                input.clear();
            }
            Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            f(&self.subscriber);
        }
        fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            f(&mut self.subscriber);
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            f(PortMut::Subscriber(&mut self.subscriber));
        }
    }

    struct NoAllocSubscriber {
        subscriber: Subscriber<u64>,
        messages_received: Arc<AtomicUsize>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target_count: usize,
    }

    impl Callback for NoAllocSubscriber {
        fn run(&mut self, _ctx: &Context) -> Run {
            let _input = RequiredInput::<u64>::new(&self.subscriber);
            let count = self.messages_received.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= self.target_count
                && let Some(signal) = self.stop_signal.get()
            {
                signal.request_stop();
            }
            Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            f(&self.subscriber);
        }
        fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            f(&mut self.subscriber);
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            f(PortMut::Subscriber(&mut self.subscriber));
        }
    }

    /// A subscriber whose `run` deliberately spins so a mid-run re-trigger from
    /// a fast publisher has a wide window to land while the node is running.
    struct SpinningSubscriber {
        subscriber: Subscriber<u64>,
        messages_received: Arc<AtomicUsize>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target_count: usize,
        spin: time::Duration,
    }

    impl Callback for SpinningSubscriber {
        fn run(&mut self, _ctx: &Context) -> Run {
            // Widen the window during which a concurrent publisher trigger
            // must be handled as a deferred re-run rather than a re-borrow.
            sleep(self.spin);
            let _input = RequiredInput::<u64>::new(&self.subscriber);
            let count = self.messages_received.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= self.target_count
                && let Some(signal) = self.stop_signal.get()
            {
                signal.request_stop();
            }
            Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            f(&self.subscriber);
        }
        fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            f(&mut self.subscriber);
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            f(PortMut::Subscriber(&mut self.subscriber));
        }
    }

    fn build_logging_executor(
        target: usize,
        stop_signal_cell: &Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        level: ExecutionLogLevel,
    ) -> (
        LiveExecutor,
        Arc<Mutex<Vec<task::execution_log::ExecutionLogMessage>>>,
        Arc<AtomicUsize>,
    ) {
        let messages_received = Arc::new(AtomicUsize::new(0));

        let publisher_node = CallbackBuilder::new(
            "LoggingPublisher".into(),
            Box::new(NoAllocPublisher {
                publisher: Publisher::<u64>::new(OutputKind::Default.into()),
                value: 0,
            }),
        )
        .with_publisher_channels(&["exec_log_ch"])
        .with_next_execution_time_callback(|now| Some(now + time::Duration::from_millis(1)))
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .with_execution_log_level(level)
        .build()
        .unwrap();

        let subscriber_node = CallbackBuilder::new(
            "LoggingSubscriber".into(),
            Box::new(NoAllocSubscriber {
                subscriber: Subscriber::<u64>::new(InputKind::Required.into()),
                messages_received: messages_received.clone(),
                stop_signal: stop_signal_cell.clone(),
                target_count: target,
            }),
        )
        .with_subscriber_channels(&["exec_log_ch"])
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .with_execution_log_level(level)
        .build()
        .unwrap();

        let collected = Arc::new(Mutex::new(Vec::new()));
        let collector_node = CallbackBuilder::new(
            "ExecutionLogCollector".into(),
            Box::new(ExecutionLogCollector {
                subscriber: Subscriber::<task::execution_log::ExecutionLogMessage>::new(
                    SubscriberConfig {
                        is_optional: true,
                        capacity: 8,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: task::execution_log::EXECUTION_LOG_CHANNEL.into(),
                    },
                ),
                collected: collected.clone(),
                stop_signal: stop_signal_cell.clone(),
                target: 4,
            }),
        )
        .with_subscriber_channels(&[task::execution_log::EXECUTION_LOG_CHANNEL])
        .with_execution_duration_callback(|| time::Duration::ZERO)
        .with_execution_log_level(ExecutionLogLevel::Off)
        .build()
        .unwrap();

        let mut nodes = vec![publisher_node, subscriber_node, collector_node];
        connect_callback_nodes(&mut nodes).expect("failed to connect data nodes");

        let mut pools = vec![ThreadPoolConfig::new(1, nodes)];
        let mut log_pubs = task::execution_log::log_publishers(&pools);
        task::execution_log::connect(&mut pools, &mut log_pubs)
            .expect("failed to connect execution-log publishers");

        let exec = LiveExecutor::new_multi_pool_with_execution_log(
            pools,
            log_pubs,
            time::Duration::from_millis(1),
        );
        (exec, collected, messages_received)
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
        assert!(
            nodes[0].callback().collect_publishers()[0]
                .config()
                .channel_name
                == "integer"
        );
        assert!(
            nodes[1].callback().collect_subscribers()[0]
                .config()
                .channel_name
                == "integer"
        );

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

    #[test]
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
            Box::new(NoAllocPublisher {
                publisher: Publisher::<u64>::new(OutputKind::Default.into()),
                value: 0,
            }),
        )
        .with_publisher_channels(&["no_alloc_integer"])
        .with_next_execution_time_callback(|now| Some(now + time::Duration::from_millis(2)))
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .build()
        .unwrap();

        let subscriber_node = CallbackBuilder::new(
            "NoAllocSubscriber".into(),
            Box::new(NoAllocSubscriber {
                subscriber: Subscriber::<u64>::new(InputKind::Required.into()),
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

    #[test]
    fn test_optional_trigger_input_data_triggers_node() {
        const TARGET_COUNT: usize = 20;

        #[cfg(not(miri))]
        const DEADLINE_SECS: u64 = 10;
        #[cfg(miri)]
        const DEADLINE_SECS: u64 = 120;

        let messages_received = Arc::new(AtomicUsize::new(0));
        let stop_signal_cell = Arc::new(OnceLock::new());

        let publisher_node = CallbackBuilder::new(
            "OptionalTriggerPublisher".into(),
            Box::new(NoAllocPublisher {
                publisher: Publisher::<u64>::new(OutputKind::Default.into()),
                value: 0,
            }),
        )
        .with_publisher_channels(&["optional_trigger_ch"])
        .with_next_execution_time_callback(|now| Some(now + time::Duration::from_millis(1)))
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .build()
        .unwrap();

        let subscriber_node = CallbackBuilder::new(
            "OptionalTriggerSubscriber".into(),
            Box::new(OptionalTriggerSubscriber {
                subscriber: Subscriber::<u64>::new(SubscriberConfig {
                    is_optional: true,
                    capacity: 4,
                    is_trigger: true,
                    keep_across_runs: true,
                    channel_name: String::new(),
                }),
                messages_received: messages_received.clone(),
                stop_signal: stop_signal_cell.clone(),
                target_count: TARGET_COUNT,
            }),
        )
        .with_subscriber_channels(&["optional_trigger_ch"])
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .build()
        .unwrap();

        let mut nodes = vec![publisher_node, subscriber_node];
        connect_callback_nodes(&mut nodes).expect("failed to connect callback nodes");

        let mut exec = LiveExecutor::new(1, nodes);

        stop_signal_cell.set(exec.stop_signal()).ok();
        exec.start_threads();

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

    #[test]
    fn test_trigger_while_running_race_is_serialized() {
        const TARGET_COUNT: usize = 100;

        #[cfg(not(miri))]
        const DEADLINE_SECS: u64 = 10;
        #[cfg(miri)]
        const DEADLINE_SECS: u64 = 120;

        let messages_received = Arc::new(AtomicUsize::new(0));
        let stop_signal_cell = Arc::new(OnceLock::new());

        let publisher_node = CallbackBuilder::new(
            "RacePublisher".into(),
            Box::new(NoAllocPublisher {
                publisher: Publisher::<u64>::new(OutputKind::Default.into()),
                value: 0,
            }),
        )
        .with_publisher_channels(&["race_ch"])
        .with_next_execution_time_callback(|now| Some(now + time::Duration::from_millis(1)))
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .build()
        .unwrap();

        let subscriber_node = CallbackBuilder::new(
            "RaceSubscriber".into(),
            Box::new(SpinningSubscriber {
                subscriber: Subscriber::<u64>::new(InputKind::Required.into()),
                messages_received: messages_received.clone(),
                stop_signal: stop_signal_cell.clone(),
                target_count: TARGET_COUNT,
                spin: time::Duration::from_millis(1),
            }),
        )
        .with_subscriber_channels(&["race_ch"])
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .build()
        .unwrap();

        let mut nodes = vec![publisher_node, subscriber_node];
        connect_callback_nodes(&mut nodes).expect("failed to connect callback nodes");

        // TWO worker threads: the publisher re-triggers the subscriber every
        // ~1ms while the subscriber's run spins ~1ms, so a trigger constantly
        // lands mid-run. Before the atomic run-state machine this could send a
        // second worker to double-borrow the node across threads (undefined
        // behavior); it must now serialize and still make progress.
        let mut exec = LiveExecutor::new_multi_pool(vec![ThreadPoolConfig::new(2, nodes)]);

        stop_signal_cell.set(exec.stop_signal()).ok();
        exec.start_threads();

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

    #[test]
    fn test_arena_cleanup_many_messages() {
        const TARGET_COUNT: usize = 40;

        const DEADLINE_SECS: u64 = 120;

        let messages_received = Arc::new(AtomicUsize::new(0));
        let stop_signal_cell = Arc::new(OnceLock::new());

        let publisher_node = CallbackBuilder::new(
            "NoAllocPublisher".into(),
            Box::new(NoAllocPublisher {
                publisher: Publisher::<u64>::new(OutputKind::Default.into()),
                value: 0,
            }),
        )
        .with_publisher_channels(&["many_messages_ch"])
        .with_next_execution_time_callback(|now| Some(now + time::Duration::from_millis(2)))
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .build()
        .unwrap();

        let subscriber_node = CallbackBuilder::new(
            "NoAllocSubscriber".into(),
            Box::new(NoAllocSubscriber {
                subscriber: Subscriber::<u64>::new(InputKind::Required.into()),
                messages_received: messages_received.clone(),
                stop_signal: stop_signal_cell.clone(),
                target_count: TARGET_COUNT,
            }),
        )
        .with_subscriber_channels(&["many_messages_ch"])
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .build()
        .unwrap();

        let mut nodes = vec![publisher_node, subscriber_node];
        connect_callback_nodes(&mut nodes).expect("failed to connect callback nodes");

        let mut exec = LiveExecutor::new(1, nodes);

        stop_signal_cell.set(exec.stop_signal()).ok();
        exec.start_threads();

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

    struct ExecutionLogCollector {
        subscriber: Subscriber<task::execution_log::ExecutionLogMessage>,
        collected: Arc<Mutex<Vec<task::execution_log::ExecutionLogMessage>>>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target: usize,
    }

    impl Callback for ExecutionLogCollector {
        fn run(&mut self, _ctx: &Context) -> Run {
            let mut input =
                OptionalInput::<task::execution_log::ExecutionLogMessage>::new(&self.subscriber);
            while let Some(msg) = input.value().cloned() {
                self.collected.lock().unwrap().push(msg);
                input.clear();
            }
            let count = self.collected.lock().unwrap().len();
            if count >= self.target
                && let Some(signal) = self.stop_signal.get()
            {
                signal.request_stop();
            }
            Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            f(&self.subscriber);
        }
        fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            f(&mut self.subscriber);
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            f(PortMut::Subscriber(&mut self.subscriber));
        }
    }

    #[test]
    fn test_execution_log_recording() {
        const TARGET: usize = 20;

        #[cfg(not(miri))]
        const DEADLINE_SECS: u64 = 10;
        #[cfg(miri)]
        const DEADLINE_SECS: u64 = 120;

        let stop_signal_cell = Arc::new(OnceLock::new());
        let (mut exec, collected, _messages_received) =
            build_logging_executor(TARGET, &stop_signal_cell, ExecutionLogLevel::Whole);
        stop_signal_cell.set(exec.stop_signal()).ok();
        exec.start_threads();

        let deadline = time::Instant::now() + time::Duration::from_secs(DEADLINE_SECS);
        while exec.is_running() && time::Instant::now() < deadline {
            sleep(time::Duration::from_millis(10));
        }
        assert!(
            !exec.is_running(),
            "executor did not self-stop within {DEADLINE_SECS}s"
        );
        exec.stop_threads().expect("stop failed");

        let messages = collected.lock().unwrap();
        assert!(
            !messages.is_empty(),
            "collector received no execution-log messages"
        );

        let mut any_published = false;
        let mut any_received = false;
        for msg in messages.iter() {
            for entry in msg.entries.iter() {
                if !entry.is_valid() {
                    continue;
                }
                assert!(entry.callback_node_index == 0 || entry.callback_node_index == 1);
                for m in entry.messages.iter() {
                    if !m.is_valid() {
                        break;
                    }
                    assert!(m.header.published_at != task::time::FrameworkTime::INVALID);
                    match m.direction {
                        task::execution_log::Direction::Published => {
                            assert_eq!(m.ordinal, 0);
                            any_published = true;
                        }
                        task::execution_log::Direction::Received => {
                            assert_eq!(m.ordinal, 0);
                            any_received = true;
                        }
                    }
                }
            }
        }
        assert!(any_published, "no published headers were recorded");
        assert!(any_received, "no received headers were recorded");
    }

    struct ExecutionLogCounter {
        subscriber: Subscriber<task::execution_log::ExecutionLogMessage>,
        count: Arc<AtomicUsize>,
    }

    impl Callback for ExecutionLogCounter {
        fn run(&mut self, _ctx: &Context) -> Run {
            let mut input =
                OptionalInput::<task::execution_log::ExecutionLogMessage>::new(&self.subscriber);
            while input.value().is_some() {
                self.count.fetch_add(1, Ordering::Relaxed);
                input.clear();
            }
            Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            f(&self.subscriber);
        }
        fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            f(&mut self.subscriber);
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            f(PortMut::Subscriber(&mut self.subscriber));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_execution_log_no_alloc() {
        const TARGET: usize = 30;

        #[cfg(not(miri))]
        const DEADLINE_SECS: u64 = 10;
        #[cfg(miri)]
        const DEADLINE_SECS: u64 = 120;

        let stop_signal_cell = Arc::new(OnceLock::new());
        let messages_received = Arc::new(AtomicUsize::new(0));

        let publisher_node = CallbackBuilder::new(
            "NoAllocLoggingPublisher".into(),
            Box::new(NoAllocPublisher {
                publisher: Publisher::<u64>::new(OutputKind::Default.into()),
                value: 0,
            }),
        )
        .with_publisher_channels(&["exec_log_no_alloc_ch"])
        .with_next_execution_time_callback(|now| Some(now + time::Duration::from_millis(1)))
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .with_execution_log_level(ExecutionLogLevel::Whole)
        .build()
        .unwrap();

        let subscriber_node = CallbackBuilder::new(
            "NoAllocLoggingSubscriber".into(),
            Box::new(NoAllocSubscriber {
                subscriber: Subscriber::<u64>::new(InputKind::Required.into()),
                messages_received: messages_received.clone(),
                stop_signal: stop_signal_cell.clone(),
                target_count: TARGET,
            }),
        )
        .with_subscriber_channels(&["exec_log_no_alloc_ch"])
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .with_execution_log_level(ExecutionLogLevel::Whole)
        .build()
        .unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let collector_node = CallbackNode::new_named(
            Box::new(ExecutionLogCounter {
                subscriber: Subscriber::<task::execution_log::ExecutionLogMessage>::new(
                    SubscriberConfig {
                        is_optional: true,
                        capacity: 8,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: task::execution_log::EXECUTION_LOG_CHANNEL.into(),
                    },
                ),
                count: counter.clone(),
            }),
            "ExecutionLogCounter".into(),
        );

        let mut nodes = vec![publisher_node, subscriber_node, collector_node];
        connect_callback_nodes(&mut nodes).expect("failed to connect data nodes");

        let mut pools = vec![ThreadPoolConfig::new(1, nodes)];
        let mut log_pubs = task::execution_log::log_publishers(&pools);
        task::execution_log::connect(&mut pools, &mut log_pubs)
            .expect("failed to connect execution-log publishers");

        let mut exec = LiveExecutor::new_multi_pool_with_execution_log(
            pools,
            log_pubs,
            time::Duration::from_millis(1),
        );
        stop_signal_cell.set(exec.stop_signal()).ok();
        exec.start_threads_no_alloc();

        let deadline = time::Instant::now() + time::Duration::from_secs(DEADLINE_SECS);
        while exec.is_running() && time::Instant::now() < deadline {
            sleep(time::Duration::from_millis(10));
        }
        assert!(
            !exec.is_running(),
            "executor did not self-stop within {DEADLINE_SECS}s (stuck at {})",
            messages_received.load(Ordering::SeqCst)
        );
        exec.stop_threads().expect("stop failed");
        assert!(messages_received.load(Ordering::SeqCst) >= TARGET);
        assert!(
            counter.load(Ordering::Relaxed) > 0,
            "counter never drained a log message"
        );
    }
}
