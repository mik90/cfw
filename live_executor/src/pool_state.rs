use crossbeam::channel::{self, Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use task::CallbackSchedule;
use task::wake::{Wake, WakeHandle};

const IDLE: u8 = 0;
const QUEUED: u8 = 1;
const RUNNING: u8 = 2;
const RETRIGGERED: u8 = 3;

pub(crate) struct PoolState {
    pub thread_count: usize,
    work_tx: Sender<usize>,
    pub work_rx: Receiver<usize>,
}

struct NodeState {
    state: AtomicU8,
    pool: usize,
}

/// Scheduling metadata only: this may outlive a run, but owns no borrowed nodes.
pub(crate) struct Scheduler {
    #[cfg(feature = "iceoryx2")]
    external_stop: std::sync::OnceLock<WakeHandle>,
    pub pools: Vec<PoolState>,
    nodes: Vec<NodeState>,
    stopped: AtomicBool,
    stop_tx: Mutex<Option<Sender<()>>>,
    pub stop_rx: Receiver<()>,
}

impl Scheduler {
    pub fn new(schedules: &[CallbackSchedule], workers: &[usize]) -> Arc<Self> {
        let pools = workers
            .iter()
            .enumerate()
            .map(|(pool, &thread_count)| {
                let capacity = schedules.iter().filter(|s| s.pool == pool).count().max(1);
                let (work_tx, work_rx) = channel::bounded(capacity);
                PoolState {
                    thread_count,
                    work_tx,
                    work_rx,
                }
            })
            .collect();
        let (stop_tx, stop_rx) = channel::bounded(0);
        Arc::new(Self {
            #[cfg(feature = "iceoryx2")]
            external_stop: std::sync::OnceLock::new(),
            pools,
            nodes: schedules
                .iter()
                .map(|s| NodeState {
                    state: AtomicU8::new(IDLE),
                    pool: s.pool,
                })
                .collect(),
            stopped: AtomicBool::new(false),
            stop_tx: Mutex::new(Some(stop_tx)),
            stop_rx,
        })
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub fn request_stop(&self) {
        self.stopped.store(true, Ordering::Release);
        // Disconnecting the sole sender wakes every worker, scheduler, and waiter.
        self.stop_tx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        #[cfg(feature = "iceoryx2")]
        if let Some(wake) = self.external_stop.get() {
            wake.wake();
        }
    }

    #[cfg(feature = "iceoryx2")]
    pub fn set_external_stop(&self, wake: WakeHandle) {
        assert!(self.external_stop.set(wake).is_ok());
        if self.is_stopped() {
            self.external_stop.get().unwrap().wake();
        }
    }

    pub fn waker(self: &Arc<Self>, node: usize) -> WakeHandle {
        Arc::new(NodeWake {
            scheduler: Arc::downgrade(self),
            node,
        })
    }

    pub fn trigger(&self, node: usize) {
        if self.is_stopped() {
            return;
        }
        let state = &self.nodes[node].state;
        let mut current = state.load(Ordering::Acquire);
        loop {
            // Even a coalesced notification performs a release RMW. This both
            // verifies the node has not changed state and publishes the queue
            // write to the worker's acquire claim of this node.
            let next = match current {
                IDLE => QUEUED,
                RUNNING => RETRIGGERED,
                QUEUED | RETRIGGERED => current,
                _ => unreachable!(),
            };
            match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    if current == IDLE {
                        self.enqueue(node);
                    }
                    return;
                }
                Err(actual) => current = actual,
            }
        }
    }

    fn enqueue(&self, node: usize) {
        // At most one queue entry per node. Running nodes do not occupy the
        // queue, so a queue sized for its pool's node count cannot overflow.
        self.pools[self.nodes[node].pool]
            .work_tx
            .try_send(node)
            .expect("duplicate node scheduling");
    }

    pub fn claim(&self, node: usize) {
        assert_eq!(
            self.nodes[node].state.swap(RUNNING, Ordering::AcqRel),
            QUEUED
        );
    }

    pub fn finish(&self, node: usize) {
        match self.nodes[node].state.compare_exchange(
            RUNNING,
            IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(RETRIGGERED) => {
                // Acquire every retrigger's publication before handing the node
                // to its next worker; a plain store would break that handoff.
                assert_eq!(
                    self.nodes[node].state.swap(QUEUED, Ordering::AcqRel),
                    RETRIGGERED
                );
                self.enqueue(node);
            }
            Err(_) => unreachable!("node must be running when released"),
        }
    }
}

struct NodeWake {
    scheduler: Weak<Scheduler>,
    node: usize,
}
impl Wake for NodeWake {
    fn wake(&self) {
        if let Some(scheduler) = self.scheduler.upgrade() {
            scheduler.trigger(self.node);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_triggers_coalesce_without_hot_path_allocation() {
        let scheduler = Scheduler::new(&[CallbackSchedule::default()], &[2]);
        let wake = scheduler.waker(0);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..100 {
                wake.wake();
                wake.wake();
                let node = scheduler.pools[0].work_rx.try_recv().unwrap();
                scheduler.claim(node);
                wake.wake();
                wake.wake();
                scheduler.finish(node);
                let node = scheduler.pools[0].work_rx.try_recv().unwrap();
                scheduler.claim(node);
                scheduler.finish(node);
                assert!(scheduler.pools[0].work_rx.is_empty());
            }
        });
    }
}
