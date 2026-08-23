use crossbeam::channel::{Receiver, Sender};
use std::fmt;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use task::callback_storage::CallbackStorage;
use task::scheduling::{CallbackNodeId, ReadyNodeSink};
use task::string_interner::{CallbackNameTag, ChannelNameTag, StringInterner};
use task::time::FrameworkTime;

#[derive(Clone, Copy)]
pub(crate) struct TimeTriggeredNode {
    pub(crate) index: usize,
    pub(crate) requested_exec_time: FrameworkTime,
}

pub(crate) struct PoolState {
    pub(crate) thread_count: usize,
    pub(crate) work_tx: Sender<usize>,
    pub(crate) work_rx: Receiver<usize>,
}

/// Work-queue bookkeeping shared between trigger sources and worker threads.
///
/// Deduplication ("a node's index is in its pool's channel at most once")
/// lives in each node's atomic run state (see [`SharedCallbackNode::trigger`]
/// ), not in a side-table: a node is only sent to the channel on the
/// `Idle → Enqueued` transition, and a trigger during a run is remembered as
/// `RunningTriggered` and re-enqueued by the worker when it releases the node.
pub(crate) struct WorkRouter {
    pub(crate) pools: Vec<Arc<PoolState>>,
    pub(crate) node_to_pool: Vec<usize>,
}

impl WorkRouter {
    /// Send an already-enqueued node ID to its assigned pool. This serves both
    /// the initial `Idle → Enqueued` transition and worker re-enqueues.
    pub(crate) fn send_enqueued(&self, node: CallbackNodeId) {
        let pool = &self.pools[self.node_to_pool[node.0]];
        let _ = pool.work_tx.send(node.0);
    }
}

pub(crate) struct LiveReadyNodeSink<'a> {
    pub(crate) nodes: &'a [Arc<task::callback_storage::SharedCallbackNode>],
    pub(crate) router: &'a WorkRouter,
}

impl ReadyNodeSink for LiveReadyNodeSink<'_> {
    fn schedule(&mut self, node: CallbackNodeId) {
        if self.nodes[node.0].trigger() {
            self.router.send_enqueued(node);
        }
    }
}

pub(crate) struct SharedThreadPoolState {
    pub(crate) work_router: Arc<WorkRouter>,
    pub(crate) periodic_mutex: Mutex<()>,
    pub(crate) periodic_cond_var: Condvar,
    pub(crate) should_run: AtomicBool,
    pub(crate) worker_count: usize,
    pub(crate) barrier_count: AtomicUsize,
    pub(crate) cleanup_done: AtomicBool,
    pub(crate) shutdown_mutex: Mutex<()>,
    pub(crate) shutdown_cv: Condvar,
    /// Interned callback names for use by tasks
    pub(crate) callback_interner: StringInterner<CallbackNameTag>,
    /// Interned channel names for use by tasks
    pub(crate) channel_interner: StringInterner<ChannelNameTag>,
}

impl SharedThreadPoolState {
    pub(crate) fn request_stop(&self) {
        let _guard = self
            .periodic_mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.should_run.store(false, Ordering::Release);
        self.periodic_cond_var.notify_all();
    }
    /// Dump the executor's state for diagnostics. The node storage is passed
    /// in because it is owned by the executor's main thread, not by this
    /// shared state (worker threads only hold `clone_shared()` clones).
    pub(crate) fn fmt_nodes(
        &self,
        f: &mut fmt::Formatter<'_>,
        nodes: &CallbackStorage,
    ) -> fmt::Result {
        writeln!(f, "Should run: {}", self.should_run.load(Ordering::Relaxed))?;
        writeln!(f, "All callback nodes:")?;
        for (index, node) in nodes.iter_shared().enumerate() {
            let Some(details) = node.try_access(|node| {
                let mut out = String::new();
                let _ = writeln!(out, "\t ----------------------------------");
                let _ = writeln!(
                    out,
                    "\t Index:{}, Name: {}, Pool: {}",
                    index,
                    node.name(),
                    self.work_router.node_to_pool[index]
                );
                let _ = writeln!(out, "\t Able to run: {}", node.able_to_run());
                let _ = writeln!(
                    out,
                    "\t Subscribers request execution: {}",
                    node.subscribers_request_execution()
                );
                let _ = writeln!(out, "\t Subscribers");
                node.callback().for_each_subscriber(&mut |s| {
                    let _ = writeln!(out, "\t\t Channel: {}", s.config().channel_name);
                    let queue_info = s.queue_info();
                    let _ = writeln!(
                        out,
                        "\t\t Reader queue size: {}, writer_queue size: {}",
                        queue_info.reader_size, queue_info.writer_size
                    );
                });
                let _ = writeln!(out, "\t ----------------------------------");
                out
            }) else {
                continue;
            };
            write!(f, "{details}")?;
        }
        Ok(())
    }
}
