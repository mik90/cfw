use crossbeam::channel::{Receiver, Sender};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use task::callback::CallbackNode;
use task::executor::CallbackNodeEnqueuer;
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

pub(crate) struct EnqueueState {
    pub(crate) pools: Vec<Arc<PoolState>>,
    pub(crate) node_to_pool: Vec<usize>,
    pub(crate) node_enqueued: Vec<AtomicBool>,
}

impl EnqueueState {
    pub(crate) fn trigger_node(&self, index: usize) {
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

pub(crate) struct SharedThreadPoolState {
    pub(crate) enqueue_state: Arc<EnqueueState>,
    pub(crate) periodic_mutex: Mutex<()>,
    pub(crate) periodic_cond_var: Condvar,
    pub(crate) nodes: Vec<Arc<Mutex<CallbackNode>>>,
    pub(crate) should_run: AtomicBool,
    pub(crate) worker_count: usize,
    pub(crate) worker_liveness: Vec<Mutex<()>>,
    pub(crate) barrier_count: AtomicUsize,
    pub(crate) cleanup_done: AtomicBool,
    pub(crate) shutdown_mutex: Mutex<()>,
    pub(crate) shutdown_cv: Condvar,
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
            node.callback().for_each_subscriber(&mut |s| {
                let _ = writeln!(f, "\t\t Channel: {}", s.config().channel_name);
                let queue_info = s.queue_info();
                let _ = writeln!(
                    f,
                    "\t\t Reader queue size: {}, writer_queue size: {}",
                    queue_info.reader_size, queue_info.writer_size
                );
            });
        }
        writeln!(f, "\t ----------------------------------")?;
        Ok(())
    }
}
