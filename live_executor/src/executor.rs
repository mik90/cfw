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
    Executor, ExecutorParams, ExecutorStopSignal, ThreadPoolConfig, TimeSource, WallClock,
};
use task::publisher::Publisher;
use task::scheduling::{CallbackNodeId, NoopReadyNodeSink, ReadyNodeSink};
use task::time::FrameworkTime;

use crate::error::LiveExecutorError;
use crate::periodic::periodic_trigger_thread;
use crate::pool_state::{
    LiveReadyNodeSink, PoolState, SharedThreadPoolState, TimeTriggeredNode, WorkRouter,
};
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
    fn new_multi_pool_core(params: ExecutorParams, time_source: T) -> Self {
        let (pools, channel_interner, callback_interner) = params.into_parts();
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

        let work_router = Arc::new(WorkRouter {
            pools: pool_states,
            node_to_pool,
        });
        let shared_state = Arc::new(SharedThreadPoolState {
            work_router: work_router.clone(),
            periodic_mutex: Mutex::new(()),
            periodic_cond_var: Condvar::new(),
            should_run: true.into(),
            worker_count,
            barrier_count: AtomicUsize::new(0),
            cleanup_done: AtomicBool::new(false),
            shutdown_mutex: Mutex::new(()),
            shutdown_cv: Condvar::new(),
            channel_interner,
            callback_interner,
        });

        let nodes = CallbackStorage::from_shared(all_shared_nodes);

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

    pub fn new_multi_pool_with_time(params: ExecutorParams, time_source: T) -> Self {
        Self::new_multi_pool_core(params, time_source)
    }

    pub fn new_multi_pool_with_execution_log_and_time(
        params: ExecutorParams,
        log_publishers: Vec<Publisher<ExecutionLogMessage>>,
        flush_period: Duration,
        time_source: T,
    ) -> Self {
        if !log_publishers.is_empty() {
            debug_assert_eq!(
                log_publishers.len(),
                params.pools().iter().map(|p| p.thread_count).sum::<usize>(),
                "execution-log publisher count must equal the total worker thread count"
            );
        }
        let per_pool_scratch_cap: Vec<usize> = params
            .pools()
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

        let mut exec = Self::new_multi_pool_core(params, time_source);
        exec.log_publishers = log_publishers;
        exec.per_pool_scratch_cap = per_pool_scratch_cap;
        exec.flush_period = flush_period;
        exec
    }
}

impl LiveExecutor<WallClock> {
    pub fn new(num_threads: usize, nodes: Vec<CallbackNode>) -> Self {
        let pools = vec![ThreadPoolConfig::new(num_threads, nodes)];
        Self::new_multi_pool(ExecutorParams::new(pools))
    }

    pub fn new_multi_pool(params: ExecutorParams) -> Self {
        Self::new_multi_pool_core(params, WallClock)
    }

    pub fn new_multi_pool_with_execution_log(
        params: ExecutorParams,
        log_publishers: Vec<Publisher<ExecutionLogMessage>>,
        flush_period: Duration,
    ) -> Self {
        Self::new_multi_pool_with_execution_log_and_time(
            params,
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
            let (next, schedule) = node.access(|node| {
                let next = node.next_requested_execution_time(now);
                let schedule = next.is_some()
                    || (node.subscribers_request_execution() && node.required_inputs_ready());
                (next, schedule)
            });
            node.set_next_exec_time(next);
            if schedule {
                let mut sink = LiveReadyNodeSink {
                    nodes: self.nodes.as_shared_slice(),
                    router: &self.shared_state.work_router,
                };
                sink.schedule(CallbackNodeId(index));
            }
        }

        let has_log_publishers = !self.log_publishers.is_empty();
        let mut log_publisher_drainer = self.log_publishers.drain(..);
        let mut handles = Vec::new();
        for (pool_idx, pool_arc) in self.shared_state.work_router.pools.iter().enumerate() {
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
                    thread_name,
                    worker_nodes,
                ));
            }
        }

        handles
    }

    pub fn start_threads(&mut self) {
        let time_source = self.time_source.clone();
        self.worker_threads =
            self.start_threads_with(move |pool, shared, init, _ts, name, nodes| {
                let ts = time_source.clone();
                thread::Builder::new()
                    .name(name)
                    .spawn(move || {
                        println!("Starting thread");
                        run_executor_thread(
                            pool.as_ref(),
                            shared.as_ref(),
                            &nodes,
                            init,
                            ts.as_ref(),
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
                    // Catching panics allows us to do some cleanup before continuing the panic
                    let panic_result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let mut exec_times: VecDeque<TimeTriggeredNode> =
                                VecDeque::with_capacity(nodes.len());
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
                            while shared_state.should_run.load(Ordering::Acquire) {
                                body(
                                    shared_state.as_ref(),
                                    &nodes,
                                    &mut exec_times,
                                    time_source.as_ref(),
                                );
                            }
                        }));
                    if let Err(payload) = panic_result {
                        shared_state.request_stop();
                        std::panic::resume_unwind(payload);
                    }
                })
                .expect("spawn periodic thread"),
        );
    }

    pub fn stop_threads(&mut self) -> Result<(), Vec<usize>> {
        self.shared_state.request_stop();

        for pool in self.shared_state.work_router.pools.iter() {
            for _ in 0..pool.thread_count {
                let _ = pool.work_tx.try_send(SHUTDOWN_SENTINEL);
            }
        }

        // Every worker, including one that panicked during logger initialization,
        // processing, or final flush, parks at this barrier while retaining its
        // logger arena.
        {
            use std::sync::atomic::Ordering as O;
            let guard = self.shared_state.shutdown_mutex.lock().unwrap();
            // drop: discard the MutexGuard from wait_while immediately, releasing
            // shutdown_mutex. _ = ... would trigger let_underscore_lock.
            drop(self.shared_state.shutdown_cv.wait_while(guard, |_| {
                self.shared_state.barrier_count.load(O::Acquire) < self.shared_state.worker_count
            }));
        }

        // Stop the scheduler before touching callback storage. It can otherwise
        // enqueue a node while teardown is clearing subscriber buffers.
        let periodic_panicked = self
            .periodic_thread
            .take()
            .is_some_and(|handle| handle.join().is_err());

        // SAFETY: all workers have unwound from their protected worker bodies
        // and are parked at the cleanup barrier, and the periodic scheduler has
        // joined, so no callback-interior reference can be held or obtained.
        unsafe { self.nodes.cleanup_subscribers_with_exclusive_access() };

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
        if periodic_panicked {
            // The scheduler follows the workers in the executor's thread
            // index space used by LiveExecutorError.
            panicked_indices.push(self.shared_state.worker_count);
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
    let ctx = Context::new(
        now,
        &shared_state.channel_interner,
        &shared_state.callback_interner,
    );

    // Worker-style execution: claim the node, run the work, refresh the
    // periodic snapshot while still holding it, then release. `execute`
    // releases the node before returning, so the re-send below can only ever
    // run once the node is free (and, if a trigger arrived mid-run, already
    // back in `Enqueued`).
    let (_, reenqueue) = nodes[index].execute(now, |node_guard| {
        let mut sink = LiveReadyNodeSink {
            nodes,
            router: &shared_state.work_router,
        };
        match logger {
            Some(logger) => {
                if !logger.has_data() {
                    // Just track drop and continue
                    node_guard.drain_subscribers();
                    let _ = node_guard.run(&ctx);
                    node_guard.flush_publishers(ctx.now, &mut sink);
                    return;
                }

                match node_guard.execution_log_level() {
                    ExecutionLogLevel::Off => {
                        node_guard.drain_subscribers();
                        let _ = node_guard.run(&ctx);
                        node_guard.flush_publishers(ctx.now, &mut sink);
                    }
                    ExecutionLogLevel::Duration => {
                        node_guard.drain_subscribers();
                        let start = task::time::FrameworkTime::from_wall_clock();
                        let _ = node_guard.run(&ctx);
                        let end = task::time::FrameworkTime::from_wall_clock();
                        let duration = end.checked_duration_since(start).unwrap_or(Duration::ZERO);

                        logger.record_duration_only(index as u32, ctx.now, duration, &mut sink);

                        node_guard.flush_publishers(ctx.now, &mut sink);

                        logger.maybe_flush_period(ctx.now, &mut sink);
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

                        logger.begin_execution(index as u32, ctx.now, duration, &mut sink);

                        logger.drain_recv_into_current(&mut sink);

                        let mut logger_sink = LiveReadyNodeSink {
                            nodes,
                            router: &shared_state.work_router,
                        };
                        node_guard.flush_publishers_logged(
                            ctx.now,
                            &mut sink,
                            &mut |ordinal, header| {
                                logger.append(
                                    task::execution_log::LoggedMessage {
                                        ordinal: ordinal as u16,
                                        direction: task::execution_log::Direction::Published,
                                        header: *header,
                                    },
                                    &mut logger_sink,
                                );
                            },
                        );

                        logger.maybe_flush_period(ctx.now, &mut sink);
                    }
                }
            }
            None => {
                node_guard.drain_subscribers();
                let _ = node_guard.run(&ctx);
                node_guard.flush_publishers(ctx.now, &mut sink);
            }
        }
    });

    // `execute` already released the node; only now that it is free (and, on
    // a mid-run trigger, already `Enqueued`) feed the index back to the pool's
    // channel so a free worker can claim it immediately.
    if reenqueue {
        shared_state
            .work_router
            .send_enqueued(CallbackNodeId(index));
    }
}

fn worker_loop_core<T: TimeSource>(
    pool_state: &PoolState,
    shared_state: &SharedThreadPoolState,
    nodes: &[Arc<SharedCallbackNode>],
    init: Option<WorkerLoggerInit>,
    time_source: &T,
    mut process: impl FnMut(
        usize,
        &[Arc<SharedCallbackNode>],
        &SharedThreadPoolState,
        Option<&mut WorkerLogger>,
        FrameworkTime,
    ),
) {
    let mut logger_init = init;
    let mut logger = None;
    // Catching panics allows us to do some cleanup before continuing the panic
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if logger_init.is_some() {
            // Read the clock before transferring logger-init ownership.
            let now = time_source.now();
            logger = WorkerLogger::new(&mut logger_init, now);
        }

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

        // Commit residual logger loans before cleanup, but do not schedule
        // consumer callbacks while the executor is shutting down.
        if let Some(logger) = logger.as_mut() {
            let mut sink = NoopReadyNodeSink;
            logger.flush_remaining(time_source.now(), &mut sink);
        }
    }));
    let panic_payload = panic_result.err();
    if panic_payload.is_some() {
        shared_state.request_stop();
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

    if let Some(payload) = panic_payload {
        std::panic::resume_unwind(payload);
    }
}

fn run_executor_thread<T: TimeSource>(
    pool_state: &PoolState,
    shared_state: &SharedThreadPoolState,
    nodes: &[Arc<SharedCallbackNode>],
    init: Option<WorkerLoggerInit>,
    time_source: &T,
) {
    worker_loop_core(
        pool_state,
        shared_state,
        nodes,
        init,
        time_source,
        process_work_item,
    );
}

#[cfg(test)]
fn no_alloc_worker_loop<T: TimeSource>(
    pool_state: &PoolState,
    shared_state: &SharedThreadPoolState,
    nodes: &[Arc<SharedCallbackNode>],
    init: Option<WorkerLoggerInit>,
    time_source: &T,
) {
    worker_loop_core(
        pool_state,
        shared_state,
        nodes,
        init,
        time_source,
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
            self.start_threads_with(move |pool, shared, init, _ts, name, nodes| {
                let pool = pool.clone();
                let shared = shared.clone();
                let ts = time_source.clone();
                thread::Builder::new()
                    .name(name)
                    .spawn(move || {
                        no_alloc_worker_loop(
                            pool.as_ref(),
                            shared.as_ref(),
                            &nodes,
                            init,
                            ts.as_ref(),
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
        executor::{Executor, ExecutorParams, ExecutorStopSignal, ThreadPoolConfig, TimeSource},
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
            ExecutorParams::new(pools),
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

        let mut exec = LiveExecutor::new_multi_pool(ExecutorParams::new(vec![
            ThreadPoolConfig::new(1, pool1),
            ThreadPoolConfig::new(1, pool2),
        ]));

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
    #[cfg_attr(
        miri,
        ignore = "assert_no_alloc relies on a custom #[global_allocator], which Miri doesn't run"
    )]
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
        #[cfg(not(miri))]
        const TARGET_COUNT: usize = 20;
        #[cfg(miri)]
        const TARGET_COUNT: usize = 5;

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
        #[cfg(not(miri))]
        const TARGET_COUNT: usize = 100;
        #[cfg(miri)]
        const TARGET_COUNT: usize = 10;

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
        let pools = vec![ThreadPoolConfig::new(2, nodes)];
        let mut exec = LiveExecutor::new_multi_pool(ExecutorParams::new(pools));

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
        #[cfg(not(miri))]
        const TARGET_COUNT: usize = 40;
        #[cfg(miri)]
        const TARGET_COUNT: usize = 5;

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
        #[cfg(not(miri))]
        const TARGET: usize = 20;
        #[cfg(miri)]
        const TARGET: usize = 5;

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
    #[cfg_attr(
        miri,
        ignore = "assert_no_alloc relies on a custom #[global_allocator], which Miri doesn't run"
    )]
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
            ExecutorParams::new(pools),
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

    struct LifecycleProbe {
        runs: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for LifecycleProbe {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Callback for LifecycleProbe {
        fn run(&mut self, _ctx: &Context) -> Run {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Run::new(1)
        }
        fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
        fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
        }
        fn for_each_port_mut<'a>(&'a mut self, _f: &mut dyn FnMut(PortMut<'a>)) {}
    }

    struct PanickingCallback;

    impl Callback for PanickingCallback {
        fn run(&mut self, _ctx: &Context) -> Run {
            panic!("intentional worker panic");
        }
        fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
        fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
        }
        fn for_each_port_mut<'a>(&'a mut self, _f: &mut dyn FnMut(PortMut<'a>)) {}
    }

    struct PanicOnThread(&'static str);

    impl TimeSource for PanicOnThread {
        fn now(&self) -> task::time::FrameworkTime {
            if std::thread::current()
                .name()
                .is_some_and(|name| name.starts_with(self.0))
            {
                panic!("intentional time-source panic");
            }
            task::time::FrameworkTime::from_nanoseconds(0)
        }
    }

    struct PanicOnWorkerCall {
        call: usize,
        calls: Arc<AtomicUsize>,
    }

    impl TimeSource for PanicOnWorkerCall {
        fn now(&self) -> task::time::FrameworkTime {
            if std::thread::current()
                .name()
                .is_some_and(|name| name.starts_with("cfw_pool_"))
            {
                let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call == self.call {
                    panic!("intentional worker time-source panic on call {call}");
                }
            }
            task::time::FrameworkTime::from_nanoseconds(0)
        }
    }

    #[test]
    fn unstarted_executor_drops_callback() {
        let drops = Arc::new(AtomicUsize::new(0));
        let node = CallbackNode::new_named(
            Box::new(LifecycleProbe {
                runs: Arc::new(AtomicUsize::new(0)),
                drops: drops.clone(),
            }),
            "drop_probe".into(),
        );
        drop(LiveExecutor::new(1, vec![node]));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cleanly_stopped_executor_drops_callback() {
        let drops = Arc::new(AtomicUsize::new(0));
        let node = CallbackNode::new_named(
            Box::new(LifecycleProbe {
                runs: Arc::new(AtomicUsize::new(0)),
                drops: drops.clone(),
            }),
            "drop_probe".into(),
        );
        let mut executor = LiveExecutor::new(1, vec![node]);
        executor.start_threads();
        executor.stop_threads().expect("clean stop");
        drop(executor);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn event_only_node_does_not_run_at_startup() {
        let runs = Arc::new(AtomicUsize::new(0));
        let node = CallbackNode::new_named(
            Box::new(LifecycleProbe {
                runs: runs.clone(),
                drops: Arc::new(AtomicUsize::new(0)),
            }),
            "event_only".into(),
        );
        let mut executor = LiveExecutor::new(1, vec![node]);
        executor.start_threads();
        std::thread::sleep(time::Duration::from_millis(10));
        executor.stop_threads().expect("clean stop");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn periodic_node_runs_at_startup() {
        let runs = Arc::new(AtomicUsize::new(0));
        let schedule_checks = Arc::new(AtomicUsize::new(0));
        let mut node = CallbackNode::new_named(
            Box::new(LifecycleProbe {
                runs: runs.clone(),
                drops: Arc::new(AtomicUsize::new(0)),
            }),
            "periodic".into(),
        );
        let schedule_checks_for_callback = schedule_checks.clone();
        node.set_execution_time_callback(Box::new(move |now| {
            schedule_checks_for_callback.fetch_add(1, Ordering::SeqCst);
            Some(now + time::Duration::from_secs(60))
        }));
        let mut executor = LiveExecutor::new(1, vec![node]);
        executor.start_threads();
        let deadline = std::time::Instant::now() + time::Duration::from_secs(1);
        while runs.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(time::Duration::from_millis(1));
        }
        executor.stop_threads().expect("clean stop");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(
            schedule_checks.load(Ordering::SeqCst),
            2,
            "startup should query once, followed by one refresh after the run"
        );
    }

    #[test]
    fn worker_panic_keeps_logger_arena_alive_until_cleanup() {
        let logged_runs = Arc::new(AtomicUsize::new(0));
        let mut logged_node = CallbackNode::new_named(
            Box::new(LifecycleProbe {
                runs: logged_runs.clone(),
                drops: Arc::new(AtomicUsize::new(0)),
            }),
            "logged".into(),
        );
        logged_node
            .set_execution_time_callback(Box::new(|now| Some(now + time::Duration::from_secs(60))));
        logged_node.set_execution_duration_callback(Box::new(|| time::Duration::ZERO));
        logged_node.set_execution_log_level(ExecutionLogLevel::Duration);

        let collected = Arc::new(AtomicUsize::new(0));
        let mut collector_node = CallbackNode::new_named(
            Box::new(ExecutionLogCounter {
                subscriber: Subscriber::<task::execution_log::ExecutionLogMessage>::new(
                    SubscriberConfig {
                        is_optional: true,
                        capacity: 1,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: task::execution_log::EXECUTION_LOG_CHANNEL.into(),
                    },
                ),
                count: collected.clone(),
            }),
            "log_collector".into(),
        );
        collector_node.set_execution_log_level(ExecutionLogLevel::Off);

        let mut panic_node =
            CallbackNode::new_named(Box::new(PanickingCallback), "panicking".into());
        panic_node
            .set_execution_time_callback(Box::new(|now| Some(now + time::Duration::from_secs(60))));
        panic_node.set_execution_log_level(ExecutionLogLevel::Off);

        // The logged node is queued first. Its zero-period logger flush leaves
        // an ArenaPtr in the collector before the next queued node panics.
        let mut pools = vec![ThreadPoolConfig::new(
            1,
            vec![logged_node, collector_node, panic_node],
        )];
        let mut log_publishers = task::execution_log::log_publishers(&pools);
        task::execution_log::connect(&mut pools, &mut log_publishers)
            .expect("connect execution log");
        let mut executor = LiveExecutor::new_multi_pool_with_execution_log(
            ExecutorParams::new(pools),
            log_publishers,
            time::Duration::ZERO,
        );

        executor.start_threads();
        let deadline = std::time::Instant::now() + time::Duration::from_secs(5);
        while executor.is_running() && std::time::Instant::now() < deadline {
            std::thread::sleep(time::Duration::from_millis(1));
        }
        assert!(!executor.is_running(), "worker panic did not stop executor");
        assert_eq!(logged_runs.load(Ordering::SeqCst), 1);
        assert_eq!(collected.load(Ordering::SeqCst), 0);
        assert_eq!(executor.stop_threads(), Err(vec![0]));
        drop(executor);
    }

    #[test]
    fn logger_initialization_panic_reaches_cleanup_barrier() {
        let node = CallbackNode::new_named(
            Box::new(LifecycleProbe {
                runs: Arc::new(AtomicUsize::new(0)),
                drops: Arc::new(AtomicUsize::new(0)),
            }),
            "event_only".into(),
        );
        let mut pools = vec![ThreadPoolConfig::new(1, vec![node])];
        let mut log_publishers = task::execution_log::log_publishers(&pools);
        task::execution_log::connect(&mut pools, &mut log_publishers)
            .expect("connect execution log");
        let mut executor = LiveExecutor::new_multi_pool_with_execution_log_and_time(
            ExecutorParams::new(pools),
            log_publishers,
            time::Duration::from_secs(1),
            PanicOnThread("cfw_pool_"),
        );

        executor.start_threads();
        let deadline = std::time::Instant::now() + time::Duration::from_secs(5);
        while executor.is_running() && std::time::Instant::now() < deadline {
            std::thread::sleep(time::Duration::from_millis(1));
        }
        assert!(!executor.is_running());
        assert_eq!(executor.stop_threads(), Err(vec![0]));
    }

    #[test]
    fn logger_final_flush_panic_reaches_cleanup_barrier() {
        let node = CallbackNode::new_named(
            Box::new(LifecycleProbe {
                runs: Arc::new(AtomicUsize::new(0)),
                drops: Arc::new(AtomicUsize::new(0)),
            }),
            "event_only".into(),
        );
        let mut pools = vec![ThreadPoolConfig::new(1, vec![node])];
        let mut log_publishers = task::execution_log::log_publishers(&pools);
        task::execution_log::connect(&mut pools, &mut log_publishers)
            .expect("connect execution log");
        let calls = Arc::new(AtomicUsize::new(0));
        let mut executor = LiveExecutor::new_multi_pool_with_execution_log_and_time(
            ExecutorParams::new(pools),
            log_publishers,
            time::Duration::from_secs(1),
            PanicOnWorkerCall {
                // Initialization is the first worker call. With no queued work,
                // the second call supplies the final-flush timestamp.
                call: 2,
                calls: calls.clone(),
            },
        );

        executor.start_threads();
        assert_eq!(executor.stop_threads(), Err(vec![0]));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn periodic_thread_panic_stops_executor_and_is_reported() {
        let node = CallbackNode::new_named(
            Box::new(LifecycleProbe {
                runs: Arc::new(AtomicUsize::new(0)),
                drops: Arc::new(AtomicUsize::new(0)),
            }),
            "event_only".into(),
        );
        let pools = vec![ThreadPoolConfig::new(1, vec![node])];
        let mut executor = LiveExecutor::new_multi_pool_with_time(
            ExecutorParams::new(pools),
            PanicOnThread("cfw_periodic"),
        );

        executor.start_threads();
        let deadline = std::time::Instant::now() + time::Duration::from_secs(5);
        while executor.is_running() && std::time::Instant::now() < deadline {
            std::thread::sleep(time::Duration::from_millis(1));
        }
        assert!(!executor.is_running());
        assert_eq!(executor.stop_threads(), Err(vec![1]));
    }
}
