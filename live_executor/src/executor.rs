use crossbeam::channel;
use std::collections::VecDeque;
#[cfg(feature = "iceoryx2")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use task::callback::CallbackNode;
use task::callback_storage::{CallbackStorage, SharedCallbackNode, WorkerNodes};
use task::context::Context;
#[cfg(feature = "iceoryx2")]
use task::execution_log::EXECUTION_LOG_CHANNEL;
use task::execution_log::{self, ExecutionLogLevel, ExecutionLogMessage};
use task::executor::{
    Executor, ExecutorParams, ExecutorStopSignal, ThreadPoolConfig, TimeSource, WallClock,
};
#[cfg(feature = "iceoryx2")]
use task::generic_publisher::GenericPublisher as _;
use task::publisher::Publisher;
#[cfg(feature = "iceoryx2")]
use task::publisher::PublisherConfig;
use task::scheduling::{CallbackNodeId, NoopReadyNodeSink, ReadyNodeSink};
use task::time::FrameworkTime;

use crate::error::LiveExecutorError;
use crate::periodic::periodic_trigger_thread;
use crate::pool_state::{
    LiveReadyNodeSink, PoolState, SharedThreadPoolState, TimeTriggeredNode, WorkRouter,
};
use crate::stop_signal::StopSignal;
use crate::worker_logger::{WorkerLogger, WorkerLoggerInit};

#[cfg(feature = "iceoryx2")]
use iceoryx2::{
    port::{listener::Listener, notifier::Notifier},
    service::{ipc_threadsafe, service_name::ServiceName},
};
#[cfg(feature = "iceoryx2")]
use task::iox2::{Iox2Context, Iox2EventRegistration, Iox2OpenCtx, Iox2ReadinessMetrics};

const DEFAULT_LOG_FLUSH_PERIOD: Duration = Duration::from_millis(500);
const SHUTDOWN_SENTINEL: usize = usize::MAX;

#[cfg(feature = "iceoryx2")]
struct ReadinessRegistration {
    node_index: u32,
    subscriber_ordinal: u16,
    event: Iox2EventRegistration,
}

#[cfg(feature = "iceoryx2")]
struct ReadinessThreadResources<T: TimeSource> {
    shared: Arc<SharedThreadPoolState>,
    nodes: Vec<Arc<SharedCallbackNode>>,
    metrics: Arc<Iox2ReadinessMetrics>,
    time_source: Arc<T>,
}

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
    #[cfg(feature = "iceoryx2")]
    readiness_thread: Option<thread::JoinHandle<()>>,
    #[cfg(feature = "iceoryx2")]
    readiness_done: Option<channel::Receiver<()>>,
    #[cfg(feature = "iceoryx2")]
    shutdown_notifier: Option<Notifier<ipc_threadsafe::Service>>,
    #[cfg(feature = "iceoryx2")]
    readiness_metrics: Option<Arc<Iox2ReadinessMetrics>>,
    #[cfg(feature = "iceoryx2")]
    startup_error: Option<String>,
    /// Kept last so its node and services outlive callback ports and threads.
    #[cfg(feature = "iceoryx2")]
    iox2_context: Option<Iox2Context>,
}

impl<T: TimeSource + 'static> LiveExecutor<T> {
    #[cfg(feature = "iceoryx2")]
    fn take_event_registrations(
        &mut self,
    ) -> Result<Vec<ReadinessRegistration>, crate::error::LiveExecutorStartError> {
        let Some(context) = self.iox2_context.as_mut() else {
            return Ok(Vec::new());
        };
        let mut registrations = Vec::new();
        for (node_index, node_handle) in self.nodes.iter_shared().enumerate() {
            let result = node_handle.access(|node| {
                let node_name = node.name().to_owned();
                let mut error = None;
                let mut subscriber_ordinal = 0;
                node.callback_mut()
                    .for_each_subscriber_mut(&mut |subscriber| {
                        let ordinal = subscriber_ordinal;
                        subscriber_ordinal += 1;
                        if error.is_some() {
                            return;
                        }
                        match subscriber.iox2_take_event_registration(context) {
                            Ok(Some(event)) => match
                                (u32::try_from(node_index), u16::try_from(ordinal))
                            {
                                (Ok(node_index), Ok(subscriber_ordinal)) => {
                                    registrations.push(ReadinessRegistration {
                                        node_index,
                                        subscriber_ordinal,
                                        event,
                                    });
                                }
                                _ => error = Some(format!(
                                    "node {node_name} has an event subscriber index outside the execution log's range"
                                )),
                            },
                            Ok(None) => {}
                            Err(reason) => error = Some(reason),
                        }
                    });
                error.map_or(Ok(()), Err)
            });
            if let Err(reason) = result {
                return Err(crate::error::LiveExecutorStartError {
                    node: Some(node_handle.access(|node| node.name().to_owned())),
                    reason,
                });
            }
        }
        Ok(registrations)
    }

    #[cfg(feature = "iceoryx2")]
    fn create_readiness_logger(
        &mut self,
        now: FrameworkTime,
    ) -> Result<Option<WorkerLogger>, crate::error::LiveExecutorStartError> {
        if self.log_publishers.is_empty() {
            return Ok(None);
        }

        let mut publisher = Publisher::<ExecutionLogMessage>::new(PublisherConfig {
            capacity: 1,
            channel_name: EXECUTION_LOG_CHANNEL.into(),
        });
        for node_handle in self.nodes.iter_shared() {
            let failure = node_handle.access(|node| {
                let node_name = node.name().to_owned();
                let mut failure = None;
                node.callback_mut()
                    .for_each_subscriber_mut(&mut |subscriber| {
                        if subscriber.config().channel_name == EXECUTION_LOG_CHANNEL
                            && publisher.connect_to_subscriber(subscriber).is_err()
                        {
                            failure = Some(node_name.clone());
                        }
                    });
                failure
            });
            if let Some(node_name) = failure {
                return Err(crate::error::LiveExecutorStartError {
                    node: Some(node_name),
                    reason: "execution-log subscriber type does not match the readiness logger"
                        .into(),
                });
            }
        }
        publisher.allocate_arena();
        let mut init = Some(WorkerLoggerInit {
            publisher,
            flush_period: self.flush_period,
            scratch_capacity: 0,
        });
        let logger = WorkerLogger::new(&mut init, now)
            .expect("readiness logger was initialized with a publisher");
        Ok(Some(logger))
    }

    #[cfg(feature = "iceoryx2")]
    fn create_readiness_shutdown_ports(
        &mut self,
    ) -> Result<
        (
            Listener<ipc_threadsafe::Service>,
            Notifier<ipc_threadsafe::Service>,
        ),
        crate::error::LiveExecutorStartError,
    > {
        static NEXT_SHUTDOWN_SERVICE: AtomicU64 = AtomicU64::new(0);
        let context = self
            .iox2_context
            .as_mut()
            .expect("event endpoints require an iox2 context");
        let unique = NEXT_SHUTDOWN_SERVICE.fetch_add(1, Ordering::Relaxed);
        let text = format!("cfw-shutdown-{}-{unique}", std::process::id());
        let name = ServiceName::new(text.as_str()).map_err(|error| {
            crate::error::LiveExecutorStartError {
                node: None,
                reason: error.to_string(),
            }
        })?;
        let service = context
            .node()
            .service_builder(&name)
            .event()
            .open_or_create()
            .map_err(|error| crate::error::LiveExecutorStartError {
                node: None,
                reason: error.to_string(),
            })?;
        let notifier = service.notifier_builder().create().map_err(|error| {
            crate::error::LiveExecutorStartError {
                node: None,
                reason: error.to_string(),
            }
        })?;
        let listener = service.listener_builder().create().map_err(|error| {
            crate::error::LiveExecutorStartError {
                node: None,
                reason: error.to_string(),
            }
        })?;
        Ok((listener, notifier))
    }

    fn new_multi_pool_core(params: ExecutorParams, time_source: T) -> Self {
        #[cfg(feature = "iceoryx2")]
        let (pools, channel_interner, callback_interner, iox2_context) =
            params.into_parts_with_iox2_context();
        #[cfg(not(feature = "iceoryx2"))]
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
            #[cfg(feature = "iceoryx2")]
            readiness_thread: None,
            #[cfg(feature = "iceoryx2")]
            readiness_done: None,
            #[cfg(feature = "iceoryx2")]
            shutdown_notifier: None,
            #[cfg(feature = "iceoryx2")]
            readiness_metrics: None,
            #[cfg(feature = "iceoryx2")]
            startup_error: None,
            #[cfg(feature = "iceoryx2")]
            iox2_context,
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
    ) -> Result<Vec<thread::JoinHandle<()>>, crate::error::LiveExecutorStartError> {
        self.shared_state.barrier_count.store(0, Ordering::Release);
        self.shared_state
            .cleanup_done
            .store(false, Ordering::Release);

        #[cfg(feature = "iceoryx2")]
        let registrations = self.take_event_registrations()?;

        let now = self.time_source.now();
        #[cfg(feature = "iceoryx2")]
        let readiness_logger = if registrations.is_empty() {
            None
        } else {
            self.create_readiness_logger(now)?
        };
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

        #[cfg(feature = "iceoryx2")]
        if !registrations.is_empty() {
            let (shutdown_listener, shutdown_notifier) = self.create_readiness_shutdown_ports()?;
            let metrics = Arc::new(Iox2ReadinessMetrics::default());
            let node_handles = self.nodes.clone_shared();
            let nodes: Vec<Arc<SharedCallbackNode>> = node_handles.iter().cloned().collect();
            let shared_state = self.shared_state.clone();
            let thread_metrics = metrics.clone();
            let thread_time_source = self.time_source.clone();
            let (done_tx, done_rx) = channel::bounded(1);
            let (ready_tx, ready_rx) = channel::bounded(1);
            let ready_panic = ready_tx.clone();
            let handle = thread::Builder::new()
                .name(String::from("cfw_iox2_readiness"))
                .spawn(move || {
                    let mut logger = readiness_logger;
                    let mut attached = false;
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        iox2_readiness_thread(
                            registrations,
                            shutdown_listener,
                            ReadinessThreadResources {
                                shared: shared_state.clone(),
                                nodes,
                                metrics: thread_metrics,
                                time_source: thread_time_source,
                            },
                            ready_tx,
                            logger.as_mut(),
                            &mut attached,
                        );
                    }));
                    let panic_payload = result.err();
                    if panic_payload.is_some() {
                        shared_state.request_stop();
                        let _ = ready_panic.send(Err(String::from(
                            "readiness thread panicked during waitset setup or processing",
                        )));
                    }
                    let _ = done_tx.send(());
                    if attached {
                        let guard = shared_state.shutdown_mutex.lock().unwrap();
                        drop(shared_state.shutdown_cv.wait_while(guard, |_| {
                            !shared_state.cleanup_done.load(Ordering::Acquire)
                        }));
                    }
                    if let Some(payload) = panic_payload {
                        std::panic::resume_unwind(payload);
                    }
                })
                .map_err(|error| crate::error::LiveExecutorStartError {
                    node: None,
                    reason: error.to_string(),
                })?;
            match ready_rx.recv() {
                Ok(Ok(())) => self.readiness_thread = Some(handle),
                Ok(Err(reason)) => {
                    let _ = handle.join();
                    return Err(crate::error::LiveExecutorStartError { node: None, reason });
                }
                Err(error) => {
                    let _ = handle.join();
                    return Err(crate::error::LiveExecutorStartError {
                        node: None,
                        reason: error.to_string(),
                    });
                }
            }
            self.readiness_done = Some(done_rx);
            self.shutdown_notifier = Some(shutdown_notifier);
            self.readiness_metrics = Some(metrics);
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

        Ok(handles)
    }

    pub fn start_threads(&mut self) {
        if let Err(error) = self.try_start_threads() {
            #[cfg(feature = "iceoryx2")]
            {
                self.startup_error = Some(error.to_string());
                self.shared_state.request_stop();
            }
            #[cfg(not(feature = "iceoryx2"))]
            panic!("{error}");
        }
    }

    /// Start all executor threads and report readiness registration failures.
    pub fn try_start_threads(&mut self) -> Result<(), crate::error::LiveExecutorStartError> {
        #[cfg(feature = "iceoryx2")]
        {
            self.startup_error = None;
        }
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
            })?;

        self.spawn_periodic_thread_with(|shared_state, nodes, exec_times, time_source| {
            periodic_trigger_thread(shared_state, nodes, exec_times, time_source);
        });
        Ok(())
    }

    /// Snapshot readiness counters; returns `None` when the graph has no event subscribers.
    #[cfg(feature = "iceoryx2")]
    pub fn iox2_readiness_metrics(&self) -> Option<Arc<Iox2ReadinessMetrics>> {
        self.readiness_metrics.clone()
    }

    /// Error from a failed `start_threads` registration attempt, if one occurred.
    #[cfg(feature = "iceoryx2")]
    pub fn startup_error(&self) -> Option<&str> {
        self.startup_error.as_deref()
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

        #[cfg(feature = "iceoryx2")]
        let readiness_finished = {
            if let Some(notifier) = &self.shutdown_notifier {
                let _ = notifier.notify();
            }
            let finished = self.readiness_thread.is_some()
                && self
                    .readiness_done
                    .take()
                    .is_some_and(|done| done.recv_timeout(Duration::from_millis(100)).is_ok());
            if !finished {
                self.readiness_thread.take();
            }
            self.shutdown_notifier = None;
            finished
        };

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

        #[cfg(feature = "iceoryx2")]
        let readiness_panicked = readiness_finished
            && self
                .readiness_thread
                .take()
                .is_some_and(|handle| handle.join().is_err());

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

        #[cfg(feature = "iceoryx2")]
        if readiness_panicked {
            panicked_indices.push(self.shared_state.worker_count + 1);
        }

        if panicked_indices.is_empty() {
            Ok(())
        } else {
            Err(panicked_indices)
        }
    }
}

#[cfg(feature = "iceoryx2")]
fn iox2_readiness_thread<T: TimeSource>(
    registrations: Vec<ReadinessRegistration>,
    shutdown_listener: Listener<ipc_threadsafe::Service>,
    resources: ReadinessThreadResources<T>,
    ready: channel::Sender<Result<(), String>>,
    mut logger: Option<&mut WorkerLogger>,
    attached: &mut bool,
) {
    use iceoryx2::prelude::{CallbackProgression, WaitSetBuilder};
    use iceoryx2::waitset::WaitSetRunResult;

    let ReadinessThreadResources {
        shared,
        nodes,
        metrics,
        time_source,
    } = resources;

    let waitset = match WaitSetBuilder::new().create::<ipc_threadsafe::Service>() {
        Ok(waitset) => waitset,
        Err(error) => {
            let _ = ready.send(Err(format!(
                "failed to create iox2 event readiness waitset: {error}"
            )));
            return;
        }
    };
    let mut guards = Vec::with_capacity(registrations.len());
    for registration in &registrations {
        match waitset.attach_notification(&registration.event.listener) {
            Ok(guard) => guards.push(guard),
            Err(error) => {
                let _ = ready.send(Err(format!(
                    "failed to attach an iox2 event listener: {error}"
                )));
                return;
            }
        }
    }
    let shutdown_guard = match waitset.attach_notification(&shutdown_listener) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = ready.send(Err(format!(
                "failed to attach iox2 shutdown listener: {error}"
            )));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    *attached = true;
    let mut sink = LiveReadyNodeSink {
        nodes: &nodes,
        router: &shared.work_router,
    };

    loop {
        if !shared.should_run.load(Ordering::Relaxed) {
            break;
        }
        saturating_counter(&metrics.loop_iterations, 1);
        let wake_started = std::time::Instant::now();
        let mut scheduled = false;
        let result = waitset.wait_and_process_once(|attachment| {
            if attachment.has_event_from(&shutdown_guard) {
                shutdown_listener
                    .try_wait(|_| {})
                    .expect("failed to drain iox2 readiness shutdown notification");
                return CallbackProgression::Stop;
            }
            for (index, registration) in registrations.iter().enumerate() {
                if !attachment.has_event_from(&guards[index]) {
                    continue;
                }
                let mut folded: Vec<task::iox2::EventRecord> = Vec::new();
                registration
                    .event
                    .listener
                    .try_wait(|activation| {
                        if let Some(logger) = logger.as_deref_mut() {
                            logger.record_iox2_event(
                                registration.node_index,
                                registration.subscriber_ordinal,
                                activation.id.as_value(),
                                activation.count,
                                time_source.now(),
                                &mut NoopReadyNodeSink,
                            );
                        }
                        saturating_counter(&metrics.activations, 1);
                        saturating_counter(&metrics.total_counts, activation.count);
                        if let Some(record) = folded
                            .iter_mut()
                            .find(|record| record.event_id == activation.id)
                        {
                            record.count = record.count.saturating_add(activation.count);
                        } else {
                            folded.push(task::iox2::EventRecord {
                                event_id: activation.id,
                                count: activation.count,
                            });
                        }
                    })
                    .expect("failed to drain iox2 event listener notification");

                for record in folded {
                    if !registration.event.staging.push(record) {
                        saturating_counter(&metrics.dropped_records, 1);
                        saturating_counter(&metrics.dropped_counts, record.count);
                    }
                }
                if let Some(task::callback::SubscriberReadiness::OptionalTrigger(readiness)) =
                    &registration.event.readiness
                    && let Some(node) = readiness.optional_trigger_arrived()
                    && shared.should_run.load(Ordering::Relaxed)
                {
                    sink.schedule(node);
                    scheduled = true;
                }
            }
            CallbackProgression::Continue
        });
        if let Some(logger) = logger.as_deref_mut() {
            logger.flush_remaining(time_source.now(), &mut NoopReadyNodeSink);
        }
        saturating_counter(&metrics.wake_batches, 1);
        if scheduled {
            let nanos = wake_started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            metrics
                .max_drain_latency_ns
                .fetch_max(nanos, Ordering::Relaxed);
        }
        match result.expect("iox2 readiness waitset failed") {
            WaitSetRunResult::StopRequest
            | WaitSetRunResult::Interrupt
            | WaitSetRunResult::TerminationRequest => break,
            WaitSetRunResult::AllEventsHandled => {}
        }
    }
}

#[cfg(feature = "iceoryx2")]
fn saturating_counter(counter: &AtomicU64, amount: u64) {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        match counter.compare_exchange_weak(
            current,
            current.saturating_add(amount),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => current = observed,
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
                    node_guard.run(&ctx);
                    node_guard.flush_publishers(ctx.now, &mut sink);
                    return;
                }

                match node_guard.execution_log_level() {
                    ExecutionLogLevel::Off => {
                        node_guard.drain_subscribers();
                        node_guard.run(&ctx);
                        node_guard.flush_publishers(ctx.now, &mut sink);
                    }
                    ExecutionLogLevel::Duration => {
                        node_guard.drain_subscribers();
                        let start = task::time::FrameworkTime::from_wall_clock();
                        node_guard.run(&ctx);
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
                        node_guard.run(&ctx);
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
                node_guard.run(&ctx);
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
        self.worker_threads = self
            .start_threads_with(move |pool, shared, init, _ts, name, nodes| {
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
            })
            .expect("no-alloc test graph has no iox2 registrations");

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
        process::Command,
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicUsize, Ordering},
        },
        thread::sleep,
        time,
    };

    use task::{
        callback::{
            Callback, CallbackNode, CallbackViews, InputKind, OutputKind, PubOrSub, PubOrSubMut,
            connect_callback_nodes,
        },
        callback_builder::CallbackBuilder,
        context::Context,
        execution_log::ExecutionLogLevel,
        executor::{Executor, ExecutorParams, ExecutorStopSignal, ThreadPoolConfig, TimeSource},
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
        fn run(&mut self, _ctx: &Context) {
            let mut output = Output::<u64>::new_default(&mut self.publisher);
            *output = self.value;
            self.value = self.value.wrapping_add(1);
            output.send();
        }

        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Publisher(&self.publisher));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Publisher(&mut self.publisher));
        }
    }

    struct OptionalTriggerSubscriber {
        subscriber: Subscriber<u64>,
        messages_received: Arc<AtomicUsize>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target_count: usize,
    }

    impl Callback for OptionalTriggerSubscriber {
        fn run(&mut self, _ctx: &Context) {
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
        }

        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Subscriber(&self.subscriber));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Subscriber(&mut self.subscriber));
        }
    }

    struct NoAllocSubscriber {
        subscriber: Subscriber<u64>,
        messages_received: Arc<AtomicUsize>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target_count: usize,
    }

    impl Callback for NoAllocSubscriber {
        fn run(&mut self, _ctx: &Context) {
            let _input = RequiredInput::<u64>::new(&self.subscriber);
            let count = self.messages_received.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= self.target_count
                && let Some(signal) = self.stop_signal.get()
            {
                signal.request_stop();
            }
        }

        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Subscriber(&self.subscriber));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Subscriber(&mut self.subscriber));
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
        fn run(&mut self, _ctx: &Context) {
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
        }

        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Subscriber(&self.subscriber));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Subscriber(&mut self.subscriber));
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
        fn run(&mut self, _ctx: &Context) {
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
        }

        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Subscriber(&self.subscriber));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Subscriber(&mut self.subscriber));
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
        fn run(&mut self, _ctx: &Context) {
            let mut input =
                OptionalInput::<task::execution_log::ExecutionLogMessage>::new(&self.subscriber);
            while input.value().is_some() {
                self.count.fetch_add(1, Ordering::Relaxed);
                input.clear();
            }
        }

        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Subscriber(&self.subscriber));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Subscriber(&mut self.subscriber));
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
        fn run(&mut self, _ctx: &Context) {
            self.runs.fetch_add(1, Ordering::SeqCst);
        }
        fn for_each_pub_or_sub<'a>(&'a self, _f: &mut dyn FnMut(PubOrSub<'a>)) {}
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, _f: &mut dyn FnMut(PubOrSubMut<'a>)) {}
    }

    struct PanickingCallback;

    impl Callback for PanickingCallback {
        fn run(&mut self, _ctx: &Context) {
            panic!("intentional worker panic");
        }
        fn for_each_pub_or_sub<'a>(&'a self, _f: &mut dyn FnMut(PubOrSub<'a>)) {}
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, _f: &mut dyn FnMut(PubOrSubMut<'a>)) {}
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

    struct AllocatingCallback {
        allocated_byte_array: Box<[u8]>,
    }

    impl Default for AllocatingCallback {
        fn default() -> Self {
            AllocatingCallback {
                allocated_byte_array: Box::new([0u8]),
            }
        }
    }

    impl Callback for AllocatingCallback {
        fn run(&mut self, _ctx: &Context) {
            self.allocated_byte_array = Box::new([0u8; 10]);
        }
        fn for_each_pub_or_sub<'a>(&'a self, _f: &mut dyn FnMut(PubOrSub<'a>)) {}
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, _f: &mut dyn FnMut(PubOrSubMut<'a>)) {}
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "assert_no_alloc relies on a custom #[global_allocator], which Miri doesn't run"
    )]
    /// We expect that the assert_no_alloc crate will panic on allocs
    fn test_run_aborting_test() {
        let output = Command::new("cargo")
            .args(["test", "--", "test_no_alloc_catches_allocs", "--ignored"])
            .output()
            .expect("Failed to run subommand");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !output.status.success(),
            "Expected subcommand to fail, but it succeeded: {stdout}"
        );

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("memory allocation of 10 bytes failed"),
            "Subprocess test stderr didn't contain expected message of 'memory allocation of 10 bytes failed': {stderr}"
        );
    }

    #[test]
    #[ignore = "not enabled in normal runs since it aborts"]
    /// We expect that the assert_no_alloc crate will abort on allocs
    fn test_no_alloc_catches_allocs() {
        let mut allocating_node =
            CallbackNode::new_named(Box::new(AllocatingCallback::default()), "allocating".into());
        allocating_node
            .set_execution_time_callback(Box::new(|now| Some(now + time::Duration::from_secs(1))));

        let mut executor = LiveExecutor::new(1, vec![allocating_node]);

        executor.start_threads_no_alloc();
        let deadline = std::time::Instant::now() + time::Duration::from_secs(15);
        while executor.is_running() && std::time::Instant::now() < deadline {
            std::thread::sleep(time::Duration::from_millis(10));
        }
        assert!(executor.stop().is_ok());
    }

    #[cfg(feature = "iceoryx2")]
    mod iox2_readiness_tests {
        use super::*;
        use std::sync::atomic::AtomicU64;
        use std::sync::mpsc::{
            Receiver as StdReceiver, Sender as StdSender, channel as std_channel,
        };
        use std::time::Duration;
        use task::execution_log::{EXECUTION_LOG_CHANNEL, ExecutionLogMessage};
        use task::iox2::{Iox2Event, Iox2EventSubscriber, Iox2Notifier, Iox2NotifyOutput};
        use task::scheduling::{CallbackNodeId, NoopReadyNodeSink};
        use task::task_graph_builder::TaskGraphBuilder;
        use task::time::FrameworkTime;

        struct NotifierCallback(Iox2Notifier);
        impl Callback for NotifierCallback {
            fn run(&mut self, _ctx: &Context) {
                Iox2NotifyOutput::new(&mut self.0).send();
            }
            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Publisher(&self.0));
            }
            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Publisher(&mut self.0));
            }
        }

        struct NativeGatePublisher(Publisher<u64>);
        impl Callback for NativeGatePublisher {
            fn run(&mut self, _ctx: &Context) {
                let mut output = Output::new_default(&mut self.0);
                *output = 1;
                output.send();
            }
            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Publisher(&self.0));
            }
            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Publisher(&mut self.0));
            }
        }

        struct EventConsumer {
            subscriber: Iox2EventSubscriber,
            required: Option<Subscriber<u64>>,
            observed: StdSender<u64>,
            entered: Option<StdSender<()>>,
            release: Option<StdReceiver<()>>,
            runs: Arc<AtomicUsize>,
        }
        impl Callback for EventConsumer {
            fn run(&mut self, _ctx: &Context) {
                let view = Iox2Event::new(&self.subscriber);
                let count = view.count();
                drop(view);
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                    if let Some(release) = self.release.take() {
                        let _ = release.recv();
                    }
                }
                self.runs.fetch_add(1, Ordering::SeqCst);
                if count > 0 {
                    let _ = self.observed.send(count);
                }
            }
            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Subscriber(&self.subscriber));
                if let Some(required) = &self.required {
                    f(PubOrSub::Subscriber(required));
                }
            }
            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Subscriber(&mut self.subscriber));
                if let Some(required) = &mut self.required {
                    f(PubOrSubMut::Subscriber(required));
                }
            }
        }

        struct ExecutionLogCollector {
            subscriber: Subscriber<ExecutionLogMessage>,
            observed: StdSender<ExecutionLogMessage>,
        }

        impl Callback for ExecutionLogCollector {
            fn run(&mut self, _ctx: &Context) {
                let mut buffer = self.subscriber.read_buffer();
                for message in buffer.as_slice() {
                    let _ = self.observed.send(message.message);
                }
                while !buffer.is_empty() {
                    buffer.pop_front();
                }
            }

            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Subscriber(&self.subscriber));
            }

            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Subscriber(&mut self.subscriber));
            }
        }

        fn cb(name: &str, callback: Box<dyn Callback>) -> CallbackBuilder {
            CallbackBuilder::new(name.into(), callback)
                .with_execution_duration_callback(|| Duration::from_millis(1))
        }

        fn event_graph(
            channel: &str,
            observed: StdSender<u64>,
            entered: Option<StdSender<()>>,
            release: Option<StdReceiver<()>>,
            runs: Arc<AtomicUsize>,
        ) -> task::task_graph_builder::BuiltTaskGraph {
            let gate_channel = format!("{channel}_required");
            TaskGraphBuilder::new()
                .add_pool(1, |pool| {
                    pool.add_callback_builder(
                        cb(
                            "iox2_notifier",
                            Box::new(NotifierCallback(Iox2Notifier::new(
                                task::publisher::PublisherConfig {
                                    capacity: 1,
                                    channel_name: channel.into(),
                                },
                            ))),
                        )
                        .with_publisher_channels(&[channel])
                        .with_periodic_execution(Duration::from_millis(5)),
                    )
                    .add_callback_builder(
                        cb(
                            "native_gate_publisher",
                            Box::new(NativeGatePublisher(Publisher::new(
                                task::publisher::PublisherConfig {
                                    capacity: 1,
                                    channel_name: gate_channel.clone(),
                                },
                            ))),
                        )
                        .with_publisher_channels(&[gate_channel.as_str()])
                        .with_periodic_execution(Duration::from_millis(10)),
                    )
                    .add_callback_builder(
                        cb(
                            "iox2_event_consumer",
                            Box::new(EventConsumer {
                                subscriber: Iox2EventSubscriber::new(
                                    task::subscriber::SubscriberConfig {
                                        is_optional: true,
                                        capacity: 32,
                                        is_trigger: true,
                                        keep_across_runs: true,
                                        channel_name: channel.into(),
                                    },
                                ),
                                required: Some(Subscriber::new(SubscriberConfig {
                                    is_optional: false,
                                    capacity: 1,
                                    is_trigger: false,
                                    keep_across_runs: true,
                                    channel_name: format!("{channel}_required"),
                                })),
                                observed,
                                entered,
                                release,
                                runs,
                            }),
                        )
                        .with_subscriber_channels(&[channel, gate_channel.as_str()]),
                    )
                })
                .build()
                .expect("event graph builds")
        }

        fn recv_before<T>(rx: &StdReceiver<T>, duration: Duration) -> T {
            rx.recv_timeout(duration)
                .expect("event readiness did not run before deadline")
        }

        fn executor_params(mut graph: task::task_graph_builder::BuiltTaskGraph) -> ExecutorParams {
            ExecutorParams::new(std::mem::take(&mut graph.pools))
                .with_iox2_context(graph.iox2_context.take())
        }

        /// A notifier activation wakes the single waitset thread and schedules its subscriber.
        #[test]
        #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
        fn readiness_thread_schedules_event_runs() {
            let (tx, rx) = std_channel();
            let mut executor = LiveExecutor::new_multi_pool(executor_params(event_graph(
                "live_ready_run",
                tx,
                None,
                None,
                Arc::new(AtomicUsize::new(0)),
            )));
            assert!(executor.try_start_threads().is_ok());
            assert!(recv_before(&rx, Duration::from_secs(3)) > 0);
            assert!(executor.iox2_readiness_metrics().unwrap().snapshot().1 > 0);
            executor.stop_threads().unwrap();
        }

        #[test]
        #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
        fn readiness_logs_each_event_recipient_in_execution_log() {
            use std::collections::HashSet;

            static NEXT_CHANNEL: AtomicU64 = AtomicU64::new(0);
            let channel = format!(
                "live_event_log_{}_{}",
                std::process::id(),
                NEXT_CHANNEL.fetch_add(1, Ordering::Relaxed)
            );
            let (first_tx, first_rx) = std_channel();
            let (second_tx, second_rx) = std_channel();
            let (log_tx, log_rx) = std_channel();
            let make_consumer = |observed: StdSender<u64>| {
                Box::new(EventConsumer {
                    subscriber: Iox2EventSubscriber::new(SubscriberConfig {
                        is_optional: true,
                        capacity: 16,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: channel.clone(),
                    }),
                    required: None,
                    observed,
                    entered: None,
                    release: None,
                    runs: Arc::new(AtomicUsize::new(0)),
                }) as Box<dyn Callback>
            };
            let mut built = TaskGraphBuilder::new()
                .add_pool(1, |pool| {
                    pool.add_callback_builder(
                        cb(
                            "event_source",
                            Box::new(NotifierCallback(Iox2Notifier::new(
                                task::publisher::PublisherConfig {
                                    capacity: 1,
                                    channel_name: channel.clone(),
                                },
                            ))),
                        )
                        .with_periodic_execution(Duration::from_millis(20)),
                    )
                    .add_callback_builder(cb("event_first", make_consumer(first_tx)))
                    .add_callback_builder(cb("event_second", make_consumer(second_tx)))
                    .add_callback_builder(
                        cb(
                            "event_log_collector",
                            Box::new(ExecutionLogCollector {
                                subscriber: Subscriber::new(SubscriberConfig {
                                    is_optional: true,
                                    capacity: 32,
                                    is_trigger: false,
                                    keep_across_runs: false,
                                    channel_name: EXECUTION_LOG_CHANNEL.into(),
                                }),
                                observed: log_tx,
                            }),
                        )
                        .with_periodic_execution(Duration::from_millis(5)),
                    )
                })
                .build()
                .unwrap();
            let log_publishers = std::mem::take(&mut built.execution_log_publishers);
            let mut executor = LiveExecutor::new_multi_pool_with_execution_log(
                executor_params(built),
                log_publishers,
                Duration::from_millis(50),
            );
            executor.try_start_threads().unwrap();
            assert!(recv_before(&first_rx, Duration::from_secs(3)) > 0);
            assert!(recv_before(&second_rx, Duration::from_secs(3)) > 0);

            let mut recipients = HashSet::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while recipients.len() < 2 {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let batch = log_rx
                    .recv_timeout(remaining)
                    .expect("readiness event did not reach the execution-log subscriber");
                for entry in batch.entries.iter().take_while(|entry| entry.is_valid()) {
                    let Some(event) = entry.iox2_event else {
                        continue;
                    };
                    assert!(matches!(entry.callback_node_index, 1 | 2));
                    assert_eq!(event.subscriber_ordinal, 0);
                    assert_eq!(event.event_id, 0);
                    assert!(event.count > 0);
                    assert_ne!(event.observed_at, FrameworkTime::INVALID);
                    assert_eq!(entry.execution_time, event.observed_at);
                    recipients.insert(entry.callback_node_index);
                }
            }
            assert_eq!(recipients, HashSet::from([1, 2]));
            executor.stop_threads().unwrap();
        }

        #[test]
        #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
        fn readiness_logger_survives_cleanup_of_unread_log_samples() {
            let channel = format!("live_readiness_cleanup_{}", std::process::id());
            let (observed_tx, observed_rx) = std_channel();
            let (unused_log_tx, _unused_log_rx) = std_channel();
            let mut built = TaskGraphBuilder::new()
                .add_pool(1, |pool| {
                    pool.add_callback_builder(
                        cb(
                            "cleanup_event_source",
                            Box::new(NotifierCallback(Iox2Notifier::new(
                                task::publisher::PublisherConfig {
                                    capacity: 1,
                                    channel_name: channel.clone(),
                                },
                            ))),
                        )
                        .with_periodic_execution(Duration::from_millis(20)),
                    )
                    .add_callback_builder(cb(
                        "cleanup_event_consumer",
                        Box::new(EventConsumer {
                            subscriber: Iox2EventSubscriber::new(SubscriberConfig {
                                is_optional: true,
                                capacity: 8,
                                is_trigger: true,
                                keep_across_runs: true,
                                channel_name: channel,
                            }),
                            required: None,
                            observed: observed_tx,
                            entered: None,
                            release: None,
                            runs: Arc::new(AtomicUsize::new(0)),
                        }),
                    ))
                    .add_callback_builder(cb(
                        "unread_execution_log",
                        Box::new(ExecutionLogCollector {
                            subscriber: Subscriber::new(SubscriberConfig {
                                is_optional: true,
                                capacity: 8,
                                is_trigger: false,
                                keep_across_runs: false,
                                channel_name: EXECUTION_LOG_CHANNEL.into(),
                            }),
                            observed: unused_log_tx,
                        }),
                    ))
                })
                .build()
                .unwrap();
            let log_publishers = std::mem::take(&mut built.execution_log_publishers);
            let mut executor = LiveExecutor::new_multi_pool_with_execution_log(
                executor_params(built),
                log_publishers,
                Duration::from_millis(50),
            );
            executor.try_start_threads().unwrap();
            recv_before(&observed_rx, Duration::from_secs(3));
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                let waiting = executor.nodes.iter_shared().nth(2).unwrap().access(|node| {
                    node.callback().collect_subscribers()[0]
                        .queue_info()
                        .writer_size
                });
                if waiting > 0 {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "no unread log sample arrived"
                );
                std::thread::yield_now();
            }
            executor.stop_threads().unwrap();
        }

        /// A staged event is included in startup seeding before the readiness thread starts.
        #[test]
        #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
        fn notify_before_thread_start_is_observed() {
            let (tx, rx) = std_channel();
            let built = event_graph(
                "live_ready_prestart",
                tx,
                None,
                None,
                Arc::new(AtomicUsize::new(0)),
            );
            for index in 0..2 {
                built.pools[0]
                    .nodes
                    .get(CallbackNodeId(index))
                    .unwrap()
                    .access(|node| node.set_execution_time_callback(Box::new(|_| None)));
            }
            let injector = built.pools[0]
                .nodes
                .get(CallbackNodeId(2))
                .unwrap()
                .access(|node| {
                    let mut injector = None;
                    node.callback_mut()
                        .for_each_subscriber_mut(&mut |subscriber| {
                            if let Some(event) =
                                subscriber.as_any().downcast_mut::<Iox2EventSubscriber>()
                            {
                                injector = Some(event.injector());
                            }
                        });
                    injector.unwrap()
                });
            let names = task::string_interner::ChannelNameInterner::default();
            let callbacks = task::string_interner::CallbackNameInterner::default();
            let ctx = Context::new(FrameworkTime::from_nanoseconds(1), &names, &callbacks);
            built.pools[0]
                .nodes
                .get(CallbackNodeId(1))
                .unwrap()
                .access(|node| {
                    node.run(&ctx);
                    node.flush_publishers(ctx.now, &mut NoopReadyNodeSink);
                });
            injector.notify(
                iceoryx2::prelude::EventId::new(0),
                7,
                &mut NoopReadyNodeSink,
            );
            let mut executor = LiveExecutor::new_multi_pool(executor_params(built));
            assert!(executor.try_start_threads().is_ok());
            assert_eq!(recv_before(&rx, Duration::from_secs(3)), 7);
            executor.stop_threads().unwrap();
        }

        /// An event arriving during a callback run defers work until the node is released.
        #[test]
        #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
        fn event_during_run_defers_rerun() {
            let (observed_tx, observed_rx) = std_channel();
            let (entered_tx, entered_rx) = std_channel();
            let (release_tx, release_rx) = std_channel();
            let runs = Arc::new(AtomicUsize::new(0));
            let mut executor = LiveExecutor::new_multi_pool(executor_params(event_graph(
                "live_ready_rerun",
                observed_tx,
                Some(entered_tx),
                Some(release_rx),
                runs.clone(),
            )));
            assert!(executor.try_start_threads().is_ok());
            recv_before(&entered_rx, Duration::from_secs(3));
            let names = task::string_interner::ChannelNameInterner::default();
            let callbacks = task::string_interner::CallbackNameInterner::default();
            let ctx = Context::new(FrameworkTime::from_nanoseconds(7), &names, &callbacks);
            executor.nodes.iter_shared().next().unwrap().access(|node| {
                node.run(&ctx);
                node.flush_publishers(ctx.now, &mut NoopReadyNodeSink);
            });
            release_tx.send(()).unwrap();
            recv_before(&observed_rx, Duration::from_secs(3));
            let second_deadline = std::time::Instant::now() + Duration::from_secs(3);
            while runs.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < second_deadline {
                std::thread::yield_now();
            }
            assert!(runs.load(Ordering::SeqCst) >= 2);
            executor.stop_threads().unwrap();
        }

        /// Shutdown notification interrupts the indefinite wait and joins the readiness thread.
        #[test]
        #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
        fn shutdown_wakes_readiness_thread() {
            let (tx, rx) = std_channel();
            let mut executor = LiveExecutor::new_multi_pool(executor_params(event_graph(
                "live_ready_shutdown",
                tx,
                None,
                None,
                Arc::new(AtomicUsize::new(0)),
            )));
            executor.try_start_threads().unwrap();
            recv_before(&rx, Duration::from_secs(3));
            let started = std::time::Instant::now();
            executor.stop_threads().unwrap();
            assert!(started.elapsed() < Duration::from_millis(500));
        }

        /// A graph without event subscribers creates neither readiness metrics nor a waitset thread.
        #[test]
        #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
        fn zero_event_graph_spawns_no_thread() {
            let mut executor = LiveExecutor::new_multi_pool(ExecutorParams::new(vec![]));
            assert!(executor.iox2_readiness_metrics().is_none());
            executor.try_start_threads().unwrap();
            executor.stop_threads().unwrap();
        }

        /// Invalid non-optional readiness fails startup and identifies the callback node.
        #[test]
        #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
        fn invalid_event_readiness_is_a_named_start_error() {
            let channel = "live_ready_bad_registration";
            let mut subscriber = Iox2EventSubscriber::new(task::subscriber::SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: channel.into(),
            });
            task::generic_subscriber::GenericSubscriber::config_mut(&mut subscriber).is_optional =
                false;
            task::generic_subscriber::GenericSubscriber::config_mut(&mut subscriber).is_trigger =
                false;
            let built = TaskGraphBuilder::new()
                .add_pool(1, |pool| {
                    pool.add_callback_builder(
                        cb(
                            "bad_event_node",
                            Box::new(EventConsumer {
                                subscriber,
                                required: None,
                                observed: std_channel().0,
                                entered: None,
                                release: None,
                                runs: Arc::new(AtomicUsize::new(0)),
                            }),
                        )
                        .with_subscriber_channels(&[channel]),
                    )
                })
                .build()
                .unwrap();
            let mut executor = LiveExecutor::new_multi_pool(executor_params(built));
            let error = executor.try_start_threads().unwrap_err();
            assert_eq!(error.node.as_deref(), Some("bad_event_node"));
        }

        /// Stopping detaches the waitset before later notifications can schedule callbacks.
        #[test]
        #[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
        fn no_schedule_after_stop() {
            let (tx, rx) = std_channel();
            let runs = Arc::new(AtomicUsize::new(0));
            let built = event_graph("live_ready_no_after_stop", tx, None, None, runs.clone());
            let mut executor = LiveExecutor::new_multi_pool(executor_params(built));
            executor.try_start_threads().unwrap();
            recv_before(&rx, Duration::from_secs(3));
            executor.stop_threads().unwrap();
            let before = runs.load(Ordering::SeqCst);
            let wake_batches = executor.iox2_readiness_metrics().unwrap().snapshot().0;
            let names = task::string_interner::ChannelNameInterner::default();
            let callbacks = task::string_interner::CallbackNameInterner::default();
            let ctx = Context::new(FrameworkTime::from_nanoseconds(9), &names, &callbacks);
            executor.nodes.iter_shared().next().unwrap().access(|node| {
                node.run(&ctx);
                node.flush_publishers(ctx.now, &mut NoopReadyNodeSink);
            });
            assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
            assert_eq!(runs.load(Ordering::SeqCst), before);
            assert_eq!(
                executor.iox2_readiness_metrics().unwrap().snapshot().0,
                wake_batches
            );
        }
    }
}
