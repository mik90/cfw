use crossbeam::channel::{self, Receiver, Sender};
use std::collections::VecDeque;
use std::fmt;
use std::num::Saturating;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::Duration;
use task::execution_log::{
    self, Direction, ENTRIES_PER_MESSAGE, ExecutionLogMessage, LoggedMessage, MESSAGES_PER_ENTRY,
};
use task::message::MessageHeader;
use task::publisher::{GenericPublisher, Publisher};
use task::time::FrameworkTime;

use task::callback::CallbackNode;
use task::context::Context;
use task::executor::{CallbackNodeEnqueuer, Executor, ExecutorStopSignal, ThreadPoolConfig};

/// Default period between execution-log message flushes when logging is enabled.
const DEFAULT_LOG_FLUSH_PERIOD: Duration = Duration::from_millis(500);

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
                node.name(),
                self.enqueue_state.node_to_pool[index]
            )?;
            writeln!(f, "\t Able to run: {}", node.able_to_run())?;
            writeln!(
                f,
                "\t Subscribers request execution: {}",
                node.subscribers_request_execution()
            )?;
            writeln!(f, "\t Subscribers")?;
            for s in node.subscribers().iter() {
                writeln!(f, "\t\t Channel: {}", s.config().channel_name)?;
                let queue_info = s.queue_info();
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
    /// Per-worker execution-log publishers, consumed (moved into worker threads)
    /// by `start_threads`. `None` when logging is off.
    log_publishers: Option<Vec<Arc<Mutex<Publisher<ExecutionLogMessage>>>>>,
    /// Per-pool scratch capacity for the worst-case received-headers count;
    /// used to size each worker's reuse buffer at thread start.
    per_pool_scratch_cap: Vec<usize>,
    /// Execution-log flush period (default 500ms).
    flush_period: Duration,
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
            .next_requested_execution_time(now)
            .unwrap_or(task::time::FrameworkTime::MAX);

        for node in exec_times.iter_mut() {
            if node.index == time_triggered_node.index {
                node.requested_exec_time = next_exec_time;
            }
        }
    }
}

/// Per-worker execution-log writer. Owns one [`Publisher<ExecutionLogMessage>`]
/// (the executor publishes one log message per thread on `execution_log`),
/// plus a loaned "current" message filled across executions. All capture state
/// is worker-local, so the hot path needs no mutual exclusion.
struct WorkerLogger {
    publisher: Arc<Mutex<Publisher<ExecutionLogMessage>>>,
    flush_period: Duration,
    last_flush: FrameworkTime,
    /// Index into the publisher's `loaned_values` of the message being filled,
    /// or `None` if no message is loaned (a flush failed to re-loan; record
    /// drops until a loan succeeds).
    current_loan: Option<usize>,
    /// The current execution's metadata, written into each entry opened for it.
    /// Cleared on flush so a stale execution can't bleed into an empty message.
    cur_node: u32,
    cur_time: FrameworkTime,
    cur_duration: Duration,
    /// First free entry slot in the current message.
    next_entry: usize,
    /// First free message slot in the current entry.
    next_msg: usize,
    /// Execution logs dropped (couldn't be recorded) while filling the current
    /// message. Stamped into it on publish and reset, so drops carry across a
    /// failed publish to the next successful one.
    dropped: Saturating<usize>,
    /// Reused, fixed-capacity buffer for received headers, sized so the capture
    /// path (under `assert_no_alloc`) never reallocates.
    recv_scratch: Vec<LoggedMessage>,
    /// Scratch length valid for the in-progress append; recv headers are staged
    /// here between snapshot and write so they can be written after `run()`
    /// (once the execution duration is known).
    recv_scratch_len: usize,
}

/// Data moved into a worker thread to construct a [`WorkerLogger`] at thread
/// start (so the scratch `Vec` is allocated on the worker, not the caller).
struct WorkerLoggerInit {
    publisher: Arc<Mutex<Publisher<ExecutionLogMessage>>>,
    flush_period: Duration,
    scratch_capacity: usize,
}

impl WorkerLogger {
    fn new(init: WorkerLoggerInit) -> Self {
        let publisher = init.publisher.clone();
        let current_loan = publisher.lock().unwrap().loan_default().ok();
        WorkerLogger {
            current_loan,
            publisher,
            flush_period: init.flush_period,
            last_flush: FrameworkTime::from_wall_clock(),
            cur_node: 0,
            cur_time: FrameworkTime::INVALID,
            cur_duration: Duration::ZERO,
            next_entry: 0,
            next_msg: 0,
            dropped: Saturating(0),
            recv_scratch: Vec::with_capacity(init.scratch_capacity),
            recv_scratch_len: 0,
        }
    }

    /// Whether this logger should record an execution of `node`: only when the
    /// node opted in *and* it can hold a current loan. A failed re-loan (logger
    /// subscriber too slow to release arena slots) drops the execution and
    /// counts it for the next message.
    fn captures_for(&mut self, node: &CallbackNode) -> bool {
        if !node.log_executions() {
            return false;
        }
        if self.current_loan.is_none() {
            self.current_loan = self.publisher.lock().unwrap().loan_default().ok();
        }
        if self.current_loan.is_some() {
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    fn recv_scratch_clear(&mut self) {
        self.recv_scratch_len = 0;
    }

    fn recv_push(&mut self, msg: LoggedMessage) {
        if self.recv_scratch_len < self.recv_scratch.len() {
            self.recv_scratch[self.recv_scratch_len] = msg;
        } else {
            self.recv_scratch.push(msg);
        }
        self.recv_scratch_len += 1;
    }

    fn begin_execution(&mut self, node_index: u32, time: FrameworkTime, duration: Duration) {
        self.cur_node = node_index;
        self.cur_time = time;
        self.cur_duration = duration;
    }

    /// Append a logged message to the current entry, opening a fresh entry (or
    /// flushing + re-loaning a fresh message) when the current one fills.
    fn append(&mut self, msg: LoggedMessage) {
        let Some(loan) = self.current_loan else {
            // Mid-execution loan loss (shouldn't happen mid-append, but guard): drop tail.
            self.dropped += 1;
            return;
        };

        if self.next_msg == MESSAGES_PER_ENTRY {
            // Current entry full → advance to next entry (flush if message full).
            self.next_entry += 1;
            self.next_msg = 0;
            if self.next_entry == ENTRIES_PER_MESSAGE {
                // Message full → publish and re-loan.
                if !self.flush_current(self.cur_time) {
                    // Couldn't re-loan: drop the rest of this execution.
                    self.dropped += 1;
                    self.next_entry = 0;
                    self.next_msg = 0;
                    return;
                }
                self.next_entry = 0;
                self.next_msg = 0;
            }
        }

        // Open the entry if still default, and write the message under one lock.
        let mut pub_guard = self.publisher.lock().unwrap();
        let cur = pub_guard.loaned_payload_mut(loan);
        let entry = &mut cur.entries[self.next_entry];
        if !entry.is_valid() {
            entry.callback_node_index = self.cur_node;
            entry.execution_time = self.cur_time;
            entry.execution_duration_ns = self.cur_duration.as_nanos() as u64;
        }
        entry.messages[self.next_msg] = msg;
        self.next_msg += 1;
    }

    /// Drain staged received headers into the current message via [`append`].
    fn drain_recv_into_current(&mut self) {
        let len = self.recv_scratch_len;
        for i in 0..len {
            // Borrow checker: copy out then append to avoid a double-borrow of self.
            let msg = self.recv_scratch[i];
            self.append(msg);
        }
        self.recv_scratch_len = 0;
    }

    /// If the flush period has elapsed and the current message has data,
    /// publish it. Called after each captured execution.
    fn maybe_flush_period(&mut self, now: FrameworkTime) {
        let due = now
            .checked_duration_since(self.last_flush)
            .map(|d| d >= self.flush_period)
            .unwrap_or(false);
        if !due {
            return;
        }
        if self.next_entry > 0 || self.next_msg > 0 {
            self.flush_current(now);
        }
    }

    /// Publish the current message (if it has data), stamping the drop count,
    /// then re-loan a fresh one. Returns `true` if a new loan is active after.
    fn flush_current(&mut self, at: FrameworkTime) -> bool {
        let Some(loan) = self.current_loan else {
            return false;
        };
        if self.next_entry == 0 && self.next_msg == 0 {
            // Nothing to publish; keep the loan.
            return true;
        }

        let mut pub_guard = self.publisher.lock().unwrap();
        {
            let cur = pub_guard.loaned_payload_mut(loan);
            cur.number_of_dropped_entries = self.dropped;
        }
        self.dropped = Saturating(0);
        self.last_flush = at;

        pub_guard.mark_loan_sent(loan);
        pub_guard.flush_loaned_values(at);

        // Re-loan for the next message. Failure leaves us loan-less; the next
        // execution's captures_for retry will re-loan or drop.
        self.current_loan = pub_guard.loan_default().ok();
        self.next_entry = 0;
        self.next_msg = 0;
        self.current_loan.is_some()
    }

    /// Publish any partially-filled message. Called on worker exit.
    fn flush_remaining(&mut self, at: FrameworkTime) {
        if self.next_entry > 0 || self.next_msg > 0 {
            self.flush_current(at);
        }
    }
}

pub(crate) fn process_work_item(
    index: usize,
    shared_state: &SharedThreadPoolState,
    logger: Option<&mut WorkerLogger>,
) {
    // Clear the enqueued flag before running so any triggers that arrive during
    // execution are captured, not dropped
    shared_state.enqueue_state.node_enqueued[index].store(false, Ordering::Release);

    let mut node_guard = shared_state.nodes[index].lock().unwrap();
    let ctx = Context::new(task::time::FrameworkTime::from_wall_clock());

    match logger {
        Some(logger) => {
            if !logger.captures_for(&node_guard) {
                // Node not opted in, or no arena slot available right now.
                // Run plainly. (A failed loan already bumped the drop counter.)
                node_guard.drain_subscribers();
                let _ = node_guard.run(&ctx);
                node_guard.flush_publishers(ctx.now);
                return;
            }

            // Drain write→read so for_each_queued_input sees what the callback
            // will see, then snapshot received headers before run() consumes them.
            node_guard.drain_subscribers();
            logger.recv_scratch_clear();
            for (ordinal, sub) in node_guard.subscribers().iter().enumerate() {
                let ordinal = ordinal as u16;
                sub.for_each_queued_input(&mut |header: &MessageHeader, _payload| {
                    logger.recv_push(LoggedMessage {
                        ordinal,
                        direction: Direction::Received,
                        header: *header,
                    });
                });
            }

            let start = task::time::FrameworkTime::from_wall_clock();
            let _ = node_guard.run(&ctx);
            let end = task::time::FrameworkTime::from_wall_clock();
            let duration = end.checked_duration_since(start).unwrap_or(Duration::ZERO);

            logger.begin_execution(index as u32, ctx.now, duration);

            // Append the received headers captured above.
            logger.drain_recv_into_current();

            // Append published headers as flush stamps them (the node's own
            // publishers are the only place headers become valid).
            node_guard.flush_publishers_logged(ctx.now, &mut |ordinal, header| {
                logger.append(LoggedMessage {
                    ordinal: ordinal as u16,
                    direction: Direction::Published,
                    header: *header,
                });
            });

            logger.maybe_flush_period(ctx.now);
        }
        None => {
            node_guard.drain_subscribers();
            let _ = node_guard.run(&ctx);
            node_guard.flush_publishers(ctx.now);
        }
    }
}

fn run_executor_thread(
    pool_state: &PoolState,
    shared_state: &SharedThreadPoolState,
    mut logger: Option<WorkerLogger>,
) {
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

        process_work_item(index, shared_state, logger.as_mut());
    }

    // Flush any partially-filled log message on graceful exit so trailing
    // entries aren't lost.
    if let Some(logger) = logger.as_mut() {
        logger.flush_remaining(task::time::FrameworkTime::from_wall_clock());
    }
}

#[cfg(test)]
fn no_alloc_worker_loop(
    pool: Arc<PoolState>,
    shared: Arc<SharedThreadPoolState>,
    mut logger: Option<WorkerLogger>,
) {
    loop {
        let index = match pool.work_rx.recv() {
            Ok(SHUTDOWN_SENTINEL) => break,
            Ok(idx) => idx,
            Err(_) => break,
        };
        if !shared.should_run.load(Ordering::Relaxed) {
            break;
        }
        assert_no_alloc::assert_no_alloc(|| process_work_item(index, &shared, logger.as_mut()));
    }

    if let Some(logger) = logger.as_mut() {
        // Flush-on-exit happens outside the no-alloc window (no constraint on it).
        logger.flush_remaining(task::time::FrameworkTime::from_wall_clock());
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
            log_publishers: None,
            per_pool_scratch_cap: Vec::new(),
            flush_period: DEFAULT_LOG_FLUSH_PERIOD,
        }
    }

    /// Create a multi-pool executor that records execution logs.
    ///
    /// `log_publishers` must contain exactly one [`Publisher<ExecutionLogMessage>`]
    /// per worker thread (sum of `thread_count` across `pools`), created with
    /// [`execution_log::log_publishers`] and wired into the graph with
    /// [`execution_log::connect`] *before* this call. They are moved into the
    /// worker threads 1:1 at [`start_threads`] (each publisher has a single
    /// writer, so no synchronization is needed).
    ///
    /// `flush_period` is the wall-clock cadence at which a worker publishes a
    /// partially-filled log message (in addition to publishing on fill and on
    /// exit). Use [`DEFAULT_LOG_FLUSH_PERIOD`] for 500ms.
    pub fn new_multi_pool_with_execution_log(
        pools: Vec<ThreadPoolConfig>,
        log_publishers: Vec<Publisher<ExecutionLogMessage>>,
        flush_period: Duration,
    ) -> Self {
        let total_threads: usize = pools.iter().map(|p| p.thread_count).sum();
        assert_eq!(
            log_publishers.len(),
            total_threads,
            "execution-log publisher count must equal the total worker thread count"
        );

        let per_pool_scratch_cap: Vec<usize> = pools
            .iter()
            .map(|pool| {
                pool.nodes
                    .iter()
                    .map(execution_log::worst_case_received_count)
                    .max()
                    .unwrap_or(0)
            })
            .collect();

        let mut exec = Self::new_multi_pool(pools);
        exec.log_publishers = Some(
            log_publishers
                .into_iter()
                .map(|p| Arc::new(Mutex::new(p)))
                .collect(),
        );
        exec.per_pool_scratch_cap = per_pool_scratch_cap;
        exec.flush_period = flush_period;
        exec
    }

    fn start_threads_with(
        &mut self,
        spawn_worker: impl Fn(
            Arc<PoolState>,
            Arc<SharedThreadPoolState>,
            Option<WorkerLoggerInit>,
        ) -> thread::JoinHandle<()>,
    ) {
        for index in 0..self.shared_state.nodes.len() {
            let node = self.shared_state.nodes[index].lock().unwrap();
            if node.subscribers_request_execution() && node.able_to_run() {
                drop(node);
                self.shared_state.enqueue_state.trigger_node(index);
            }
        }

        // Take the log publishers out of self so we can move each into a worker.
        // They are assigned in pool order: pool p's workers get the publishers for
        // the range [offset_p, offset_p + thread_count_p).
        let log_publishers = self.log_publishers.as_ref();
        let mut next_pub = 0usize;

        for (pool_idx, pool_arc) in self.shared_state.enqueue_state.pools.iter().enumerate() {
            for _ in 0..pool_arc.thread_count {
                let pool = pool_arc.clone();
                let shared = self.shared_state.clone();
                let init = match log_publishers {
                    Some(pubs) => {
                        let publisher = pubs[next_pub].clone();
                        next_pub += 1;
                        Some(WorkerLoggerInit {
                            publisher,
                            flush_period: self.flush_period,
                            scratch_capacity: self.per_pool_scratch_cap[pool_idx],
                        })
                    }
                    None => None,
                };
                self.threads.push(spawn_worker(pool, shared, init));
            }
        }

        let shared_state = self.shared_state.clone();
        let thread = thread::spawn(move || {
            let now = task::time::FrameworkTime::from_wall_clock();
            let mut exec_times: VecDeque<TimeTriggeredNode> = VecDeque::new();
            for (index, node) in shared_state.nodes.iter().enumerate() {
                if let Some(t) = node.lock().unwrap().next_requested_execution_time(now) {
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
        self.start_threads_with(|pool, shared, init| {
            thread::spawn(move || {
                println!("Starting thread");
                let logger = init.map(WorkerLogger::new);
                run_executor_thread(pool.as_ref(), shared.as_ref(), logger);
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
            for subscriber in node.subscribers().iter() {
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
        self.start_threads_with(|pool, shared, init| {
            thread::spawn(move || {
                let logger = init.map(WorkerLogger::new);
                no_alloc_worker_loop(pool, shared, logger)
            })
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
            Arc, Mutex, OnceLock,
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
        input::{OptionalInput, RequiredInput},
        output::Output,
        publisher::Publisher,
        subscriber::{Subscriber, SubscriberConfig},
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
            if run_number >= self.target_runs
                && let Some(signal) = self.stop_signal.get()
            {
                signal.request_stop();
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

    /// A callback with a single optional+trigger input. It must be
    /// data-triggered by arriving messages — before optional+trigger
    /// subscribers got readiness state, such a node only ever ran at startup.
    struct OptionalTriggerSubscriber {
        messages_received: Arc<AtomicUsize>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target_count: usize,
    }

    impl Callback for OptionalTriggerSubscriber {
        fn run_generic(
            &mut self,
            subscribers: &mut [Box<dyn GenericSubscriber>],
            _publishers: &mut [Box<dyn GenericPublisher>],
            _ctx: &Context,
        ) -> Run {
            let mut input = OptionalInput::<u64>::new_downcasted(&mut *subscribers[0]);
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

        fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
            vec![Box::new(Subscriber::<u64>::new(SubscriberConfig {
                is_optional: true,
                capacity: 4,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: String::new(),
            }))]
        }

        fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
            vec![]
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
            if count >= self.target_count
                && let Some(signal) = self.stop_signal.get()
            {
                signal.request_stop();
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
        assert!(nodes[0].publishers()[0].config().channel_name == "integer");
        assert!(nodes[1].subscribers()[0].config().channel_name == "integer");

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

    /// Regression test for the optional+trigger gap: a node whose only input
    /// is optional+trigger used to run once at startup and never again on
    /// data, since no readiness bit was ever set for it. It must now be
    /// data-triggered by every arriving message (subject to queue capacity).
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
            Box::new(NoAllocPublisher { value: 0 }),
        )
        .with_publisher_channels(&["optional_trigger_ch"])
        .with_next_execution_time_callback(|now| Some(now + time::Duration::from_millis(1)))
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .build()
        .unwrap();

        let subscriber_node = CallbackBuilder::new(
            "OptionalTriggerSubscriber".into(),
            Box::new(OptionalTriggerSubscriber {
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
    fn test_arena_cleanup_many_messages() {
        const TARGET_COUNT: usize = 40;

        const DEADLINE_SECS: u64 = 120;

        let messages_received = Arc::new(AtomicUsize::new(0));
        let stop_signal_cell = Arc::new(OnceLock::new());

        let publisher_node = CallbackBuilder::new(
            "NoAllocPublisher".into(),
            Box::new(NoAllocPublisher { value: 0 }),
        )
        .with_publisher_channels(&["many_messages_ch"])
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

    /// A callback that drains `execution_log` messages into a shared vector, for
    /// integration tests that inspect what the executor recorded.
    struct ExecutionLogCollector {
        collected: Arc<Mutex<Vec<task::execution_log::ExecutionLogMessage>>>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target: usize,
    }

    impl Callback for ExecutionLogCollector {
        fn run_generic(
            &mut self,
            subscribers: &mut [Box<dyn GenericSubscriber>],
            _publishers: &mut [Box<dyn GenericPublisher>],
            _ctx: &Context,
        ) -> Run {
            let mut input =
                OptionalInput::<task::execution_log::ExecutionLogMessage>::new_downcasted(
                    &mut *subscribers[0],
                );
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

        fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
            vec![Box::new(Subscriber::<
                task::execution_log::ExecutionLogMessage,
            >::new(SubscriberConfig {
                is_optional: true,
                capacity: 8,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: task::execution_log::EXECUTION_LOG_CHANNEL.into(),
            }))]
        }

        fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
            vec![]
        }
    }

    /// Builds the graph (publisher + subscriber on an integer channel + a
    /// collector on the execution-log channel), opts the two data nodes into
    /// execution logging, and wires the executor's log publishers. Returns the
    /// executor and the shared collected-messages vector.
    fn build_logging_executor(
        target: usize,
        stop_signal_cell: &Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    ) -> (
        LiveExecutor,
        Arc<Mutex<Vec<task::execution_log::ExecutionLogMessage>>>,
        Arc<AtomicUsize>,
    ) {
        let messages_received = Arc::new(AtomicUsize::new(0));

        let publisher_node = CallbackBuilder::new(
            "LoggingPublisher".into(),
            Box::new(NoAllocPublisher { value: 0 }),
        )
        .with_publisher_channels(&["exec_log_ch"])
        .with_next_execution_time_callback(|now| Some(now + time::Duration::from_millis(1)))
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .with_execution_logging(true)
        .build()
        .unwrap();

        let subscriber_node = CallbackBuilder::new(
            "LoggingSubscriber".into(),
            Box::new(NoAllocSubscriber {
                messages_received: messages_received.clone(),
                stop_signal: stop_signal_cell.clone(),
                target_count: target,
            }),
        )
        .with_subscriber_channels(&["exec_log_ch"])
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .with_execution_logging(true)
        .build()
        .unwrap();

        let collected = Arc::new(Mutex::new(Vec::new()));
        let collector_node = CallbackBuilder::new(
            "ExecutionLogCollector".into(),
            Box::new(ExecutionLogCollector {
                collected: collected.clone(),
                stop_signal: stop_signal_cell.clone(),
                target: 4,
            }),
        )
        .with_subscriber_channels(&[task::execution_log::EXECUTION_LOG_CHANNEL])
        .with_execution_duration_callback(|| time::Duration::ZERO)
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
    fn test_execution_log_recording() {
        const TARGET: usize = 20;

        #[cfg(not(miri))]
        const DEADLINE_SECS: u64 = 10;
        #[cfg(miri)]
        const DEADLINE_SECS: u64 = 120;

        let stop_signal_cell = Arc::new(OnceLock::new());
        let (mut exec, collected, _messages_received) =
            build_logging_executor(TARGET, &stop_signal_cell);
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

        // Reassemble entries across messages, grouping by (node, execution_time).
        // A node runs on one thread at a time, so (node, time) is unique per
        // execution; split entries share it.
        let mut any_published = false;
        let mut any_received = false;
        for msg in messages.iter() {
            for entry in msg.entries.iter() {
                if !entry.is_valid() {
                    continue;
                }
                // Only our two opted-in nodes (indices 0 and 1) are logged.
                assert!(entry.callback_node_index == 0 || entry.callback_node_index == 1);
                for m in entry.messages.iter() {
                    if !m.is_valid() {
                        break;
                    }
                    assert!(m.header.published_at != task::time::FrameworkTime::INVALID);
                    match m.direction {
                        task::execution_log::Direction::Published => {
                            // Publisher node's only publisher is ordinal 0.
                            assert_eq!(m.ordinal, 0);
                            any_published = true;
                        }
                        task::execution_log::Direction::Received => {
                            // Subscriber node's only subscriber is ordinal 0.
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

    /// A no-allocation execution-log consumer: counts via an atomic, drains the
    /// read buffer without pushing into a `Vec` (which would allocate).
    struct ExecutionLogCounter {
        count: Arc<AtomicUsize>,
    }

    impl Callback for ExecutionLogCounter {
        fn run_generic(
            &mut self,
            subscribers: &mut [Box<dyn GenericSubscriber>],
            _publishers: &mut [Box<dyn GenericPublisher>],
            _ctx: &Context,
        ) -> Run {
            let mut input =
                OptionalInput::<task::execution_log::ExecutionLogMessage>::new_downcasted(
                    &mut *subscribers[0],
                );
            while input.value().is_some() {
                self.count.fetch_add(1, Ordering::Relaxed);
                input.clear();
            }
            Run::new(1)
        }

        fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
            vec![Box::new(Subscriber::<
                task::execution_log::ExecutionLogMessage,
            >::new(SubscriberConfig {
                is_optional: true,
                capacity: 4,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: task::execution_log::EXECUTION_LOG_CHANNEL.into(),
            }))]
        }

        fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
            vec![]
        }
    }

    /// A node that publishes on the execution-log channel must not be the
    /// thing we count — the executor publishes those. The counter here just
    /// drains; the publisher is the executor's internal log publisher. This
    /// test runs the *capture* path (publisher + subscriber data nodes, both
    /// opted in) plus the counter consumer under the process-wide
    /// `assert_no_alloc` global allocator, driving the worker via
    /// `start_threads_no_alloc`. The capture path must stay allocation-free.
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
            Box::new(NoAllocPublisher { value: 0 }),
        )
        .with_publisher_channels(&["exec_log_no_alloc_ch"])
        .with_next_execution_time_callback(|now| Some(now + time::Duration::from_millis(1)))
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .with_execution_logging(true)
        .build()
        .unwrap();

        let subscriber_node = CallbackBuilder::new(
            "NoAllocLoggingSubscriber".into(),
            Box::new(NoAllocSubscriber {
                messages_received: messages_received.clone(),
                stop_signal: stop_signal_cell.clone(),
                target_count: TARGET,
            }),
        )
        .with_subscriber_channels(&["exec_log_no_alloc_ch"])
        .with_execution_duration_callback(|| time::Duration::from_millis(1))
        .with_execution_logging(true)
        .build()
        .unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let collector_node = CallbackNode::new_named(
            Box::new(ExecutionLogCounter {
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
