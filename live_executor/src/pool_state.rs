use crossbeam::channel::{self, Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use task::CallbackSchedule;
use task::wake::{Wake, WakeHandle};

const IDLE: u8 = 0;
const QUEUED: u8 = 1;
const RUNNING: u8 = 2;
const RETRIGGERED: u8 = 3;
const WAITING: u8 = 4;
const READINESS_CHANGED: u8 = 5;

pub(crate) struct PoolState {
    pub thread_count: usize,
    work_tx: Sender<usize>,
    pub work_rx: Receiver<usize>,
}

struct NodeState {
    state: AtomicU8,
    pool: usize,
    deadline: AtomicI64,
}

/// Scheduling metadata only: this may outlive a run, but owns no borrowed nodes.
pub(crate) struct Scheduler {
    active: AtomicUsize,
    completed: AtomicUsize,
    timers_enabled: Mutex<bool>,
    #[cfg(feature = "iceoryx2")]
    poll_requested: AtomicUsize,
    #[cfg(feature = "iceoryx2")]
    poll_completed: AtomicUsize,
    #[cfg(feature = "iceoryx2")]
    pub event_targets: std::sync::OnceLock<
        std::collections::BTreeMap<(String, usize), crate::stop_signal::EventTarget>,
    >,
    #[cfg(feature = "iceoryx2")]
    external_stop: std::sync::OnceLock<WakeHandle>,
    pub pools: Vec<PoolState>,
    nodes: Vec<NodeState>,
    stopped: AtomicBool,
    stop_tx: Mutex<Option<Sender<()>>>,
    pub stop_rx: Receiver<()>,
    timer_tx: Sender<()>,
    pub timer_rx: Receiver<()>,
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
        let (timer_tx, timer_rx) = channel::bounded(1);
        Arc::new(Self {
            active: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            timers_enabled: Mutex::new(true),
            #[cfg(feature = "iceoryx2")]
            poll_requested: AtomicUsize::new(0),
            #[cfg(feature = "iceoryx2")]
            poll_completed: AtomicUsize::new(0),
            #[cfg(feature = "iceoryx2")]
            event_targets: std::sync::OnceLock::new(),
            #[cfg(feature = "iceoryx2")]
            external_stop: std::sync::OnceLock::new(),
            pools,
            nodes: schedules
                .iter()
                .map(|s| NodeState {
                    state: AtomicU8::new(IDLE),
                    pool: s.pool,
                    deadline: AtomicI64::new(task::time::FrameworkTime::INVALID.to_nanoseconds()),
                })
                .collect(),
            stopped: AtomicBool::new(false),
            stop_tx: Mutex::new(Some(stop_tx)),
            stop_rx,
            timer_tx,
            timer_rx,
        })
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    pub fn is_idle(&self) -> bool {
        self.active.load(Ordering::Acquire) == 0
    }
    pub fn completion_epoch(&self) -> usize {
        self.completed.load(Ordering::Acquire)
    }
    pub fn timers_enabled(&self) -> bool {
        *self
            .timers_enabled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    pub fn dispatch_due(&self, node: usize, now: task::time::FrameworkTime) {
        let enabled = self
            .timers_enabled
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *enabled && self.claim_due(node, now) {
            self.trigger(node);
        }
    }
    pub fn quiesce_timers(&self, now: task::time::FrameworkTime) {
        let mut enabled = self
            .timers_enabled
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *enabled {
            for node in 0..self.nodes.len() {
                if self.claim_due(node, now) {
                    self.trigger(node);
                }
            }
            *enabled = false;
        }
        let _ = self.timer_tx.try_send(());
    }
    #[cfg(feature = "iceoryx2")]
    pub fn request_poll(&self) -> usize {
        let ticket = self.poll_requested.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(wake) = self.external_stop.get() {
            wake.wake();
        } else {
            self.poll_completed.fetch_max(ticket, Ordering::Release);
        }
        ticket
    }
    #[cfg(feature = "iceoryx2")]
    pub fn poll_request(&self) -> usize {
        self.poll_requested.load(Ordering::Acquire)
    }
    #[cfg(feature = "iceoryx2")]
    pub fn acknowledge_poll(&self, ticket: usize) {
        self.poll_completed.fetch_max(ticket, Ordering::Release);
    }
    #[cfg(feature = "iceoryx2")]
    pub fn polled(&self, ticket: usize) -> bool {
        self.poll_completed.load(Ordering::Acquire) >= ticket
    }

    pub fn set_deadline(&self, node: usize, time: Option<task::time::FrameworkTime>) {
        self.nodes[node].deadline.store(
            time.unwrap_or(task::time::FrameworkTime::INVALID)
                .to_nanoseconds(),
            Ordering::Release,
        );
        let _ = self.timer_tx.try_send(());
    }
    pub fn deadline(&self, node: usize) -> Option<task::time::FrameworkTime> {
        let time = task::time::FrameworkTime::from_nanoseconds(
            self.nodes[node].deadline.load(Ordering::Acquire),
        );
        (time != task::time::FrameworkTime::INVALID).then_some(time)
    }
    pub fn claim_due(&self, node: usize, now: task::time::FrameworkTime) -> bool {
        let Some(time) = self.deadline(node) else {
            return false;
        };
        time <= now
            && self.nodes[node]
                .deadline
                .compare_exchange(
                    time.to_nanoseconds(),
                    task::time::FrameworkTime::INVALID.to_nanoseconds(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
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
        self.notify(node, true);
    }

    fn notify(&self, node: usize, trigger: bool) {
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
                IDLE => {
                    if trigger {
                        QUEUED
                    } else {
                        IDLE
                    }
                }
                WAITING => QUEUED,
                RUNNING | READINESS_CHANGED => {
                    if trigger {
                        RETRIGGERED
                    } else {
                        READINESS_CHANGED
                    }
                }
                QUEUED | RETRIGGERED => current,
                _ => unreachable!(),
            };
            let new_work = next == QUEUED && current != QUEUED;
            if new_work {
                self.active.fetch_add(1, Ordering::AcqRel);
            }
            match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    if next == QUEUED && current != QUEUED {
                        self.enqueue(node);
                    }
                    return;
                }
                Err(actual) => {
                    if new_work {
                        self.active.fetch_sub(1, Ordering::AcqRel);
                    }
                    current = actual;
                }
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

    pub fn finish(&self, node: usize, executed: bool) {
        let state = &self.nodes[node].state;
        let mut current = state.load(Ordering::Acquire);
        loop {
            let next = match current {
                RUNNING => {
                    if executed {
                        IDLE
                    } else {
                        WAITING
                    }
                }
                READINESS_CHANGED => {
                    if executed {
                        IDLE
                    } else {
                        QUEUED
                    }
                }
                RETRIGGERED => QUEUED,
                _ => unreachable!("node must be running when released"),
            };
            match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    if next == QUEUED {
                        self.enqueue(node);
                    } else {
                        self.completed.fetch_add(1, Ordering::AcqRel);
                        self.active.fetch_sub(1, Ordering::AcqRel);
                    }
                    return;
                }
                Err(actual) => current = actual,
            }
        }
    }
}

struct NodeWake {
    scheduler: Weak<Scheduler>,
    node: usize,
}
impl Wake for NodeWake {
    fn readiness_changed(&self) {
        if let Some(scheduler) = self.scheduler.upgrade() {
            scheduler.notify(self.node, false);
        }
    }
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
    fn readiness_changes_resume_only_an_outstanding_trigger() {
        let scheduler = Scheduler::new(&[CallbackSchedule::default()], &[1]);
        let wake = scheduler.waker(0);
        wake.readiness_changed();
        assert!(scheduler.pools[0].work_rx.is_empty());
        wake.wake();
        let node = scheduler.pools[0].work_rx.try_recv().unwrap();
        scheduler.claim(node);
        scheduler.finish(node, false); // Required data is missing.
        wake.readiness_changed();
        assert_eq!(scheduler.pools[0].work_rx.try_recv().unwrap(), node);
        scheduler.claim(node);
        wake.readiness_changed(); // Does not request another run after success.
        scheduler.finish(node, true);
        assert!(scheduler.pools[0].work_rx.is_empty());
        wake.wake();
        let node = scheduler.pools[0].work_rx.try_recv().unwrap();
        scheduler.claim(node);
        wake.readiness_changed(); // Arrival racing the failed readiness check.
        scheduler.finish(node, false);
        assert_eq!(scheduler.pools[0].work_rx.try_recv().unwrap(), node);
        scheduler.claim(node);
        scheduler.finish(node, true);
    }
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
                scheduler.finish(node, true);
                let node = scheduler.pools[0].work_rx.try_recv().unwrap();
                scheduler.claim(node);
                scheduler.finish(node, true);
                assert!(scheduler.pools[0].work_rx.is_empty());
            }
        });
    }
}
