//! Shared storage for the callback nodes of an executor.

use std::cell::UnsafeCell;
use std::ops::Index;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::callback::CallbackNode;
use crate::time::{AtomicFrameworkTime, FrameworkTime};

use crate::scheduling::CallbackNodeId;

/// The scheduling state of a [`SharedCallbackNode`], enforced atomically:
///
/// ```text
///            trigger                worker dequeues
///  Idle ───────────────▶ Enqueued ─────────────────▶ Running
///   ▲                        │                          │ │
///   │                        │ re-enqueue (trigger      │ │ worker done
///   │                        │ arrived mid-run)         │ │
///   │                        │      ◀──────────────────┘ │
///   └────────────────────────┘          trigger          │
///                                         ──────────────▶│ RunningTriggered
/// ```
///
/// A node's index is in its pool's work channel exactly while it is
/// `Enqueued`, which is what deduplicates triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum RunState {
    /// Not enqueued and not running.
    Idle = 0,
    /// The node's index is in its pool's work channel (or about to be).
    Enqueued = 1,
    /// A worker thread is currently executing the node.
    Running = 2,
    /// Running, and a trigger arrived mid-run: re-enqueue when it finishes.
    RunningTriggered = 3,
}

impl RunState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => RunState::Idle,
            1 => RunState::Enqueued,
            2 => RunState::Running,
            3 => RunState::RunningTriggered,
            other => panic!("invalid RunState value {other}"),
        }
    }
}

const IDLE: u8 = RunState::Idle as u8;
const ENQUEUED: u8 = RunState::Enqueued as u8;
const RUNNING: u8 = RunState::Running as u8;
const RUNNING_TRIGGERED: u8 = RunState::RunningTriggered as u8;

/// A [`CallbackNode`] shared between an executor's coordinating thread and its
/// worker threads.
///
/// Exclusive access to the node is enforced by the [`RunState`] protocol: only
/// the thread that transitions the node into `Running` may touch the
/// [`UnsafeCell`] interior, and only until it transitions the state back out.
/// Violations are impossible (a second worker cannot win the same CAS) or
/// panic loudly.
///
/// `next_exec_time` is a snapshot of the node's next requested execution
/// time, refreshed by the worker after each run. Scheduling threads (e.g. the
/// live executor's periodic thread) read the snapshot instead of node
/// internals.
#[derive(Debug)]
pub struct SharedCallbackNode {
    run_state: AtomicU8,
    next_exec_time: AtomicFrameworkTime,
    node: UnsafeCell<CallbackNode>,
}

struct RunningRelease<'a> {
    node: &'a SharedCallbackNode,
    active: bool,
}

impl Drop for RunningRelease<'_> {
    fn drop(&mut self) {
        if self.active {
            self.node.run_state.store(IDLE, Ordering::Release);
        }
    }
}

/// SAFETY: the `UnsafeCell` interior is only accessed by the thread that won
/// the `run_state` CAS into `Running`, so `&mut` access is never shared
/// across threads. `CallbackNode` is `Send`; this wrapper supplies synchronized
/// shared access.
unsafe impl Sync for SharedCallbackNode {}

impl SharedCallbackNode {
    pub fn new(node: CallbackNode) -> Self {
        SharedCallbackNode {
            run_state: AtomicU8::new(RunState::Idle as u8),
            next_exec_time: AtomicFrameworkTime::new(FrameworkTime::INVALID),
            node: UnsafeCell::new(node),
        }
    }

    /// Request an execution of this node. Returns `true` on `Idle → Enqueued`
    /// — the caller must then send the node's index to its pool's work
    /// channel. Returns `false` if already enqueued (deduplicated) or
    /// currently running (the re-run happens when the current run ends).
    pub fn trigger(&self) -> bool {
        loop {
            match self.run_state.compare_exchange(
                IDLE,
                ENQUEUED,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(RUNNING) => {
                    if self
                        .run_state
                        .compare_exchange(
                            RUNNING,
                            RUNNING_TRIGGERED,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        return false;
                    }
                }
                Err(ENQUEUED) | Err(RUNNING_TRIGGERED) => return false,
                Err(other) => unreachable!("run_state held invalid value {other}"),
            }
        }
    }

    /// Claim the node for execution (`Enqueued → Running`, or
    /// `Idle → Running` for direct runs). Panics if the node is already
    /// running — a scheduling protocol violation.
    fn acquire_running(&self) {
        loop {
            let prev = self.run_state.compare_exchange(
                ENQUEUED,
                RUNNING,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            match prev {
                Ok(_) => return,
                Err(IDLE) => {
                    if self
                        .run_state
                        .compare_exchange(IDLE, RUNNING, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                    {
                        return;
                    }
                }
                Err(RUNNING) | Err(RUNNING_TRIGGERED) => panic!(
                    "callback node is already running; concurrent execution is a protocol violation"
                ),
                Err(other) => unreachable!("run_state held invalid value {other}"),
            }
        }
    }

    /// Release the node after a run. Returns `true` if a trigger arrived
    /// mid-run — the caller must send the node's index back to its pool's
    /// work channel.
    fn release_running(&self) -> bool {
        loop {
            let prev = self.run_state.compare_exchange(
                RUNNING_TRIGGERED,
                ENQUEUED,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            match prev {
                Ok(_) => return true,
                Err(RUNNING) => {
                    if self
                        .run_state
                        .compare_exchange(RUNNING, IDLE, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                    {
                        return false;
                    }
                }
                Err(actual @ (IDLE | ENQUEUED)) => panic!(
                    "release_running called on a node we do not hold ({:?}); protocol violation",
                    RunState::from_u8(actual)
                ),
                Err(other) => unreachable!("run_state held invalid value {other}"),
            }
        }
    }

    /// Whether a worker currently holds the node (`Running` or
    /// `RunningTriggered`).
    pub fn is_running(&self) -> bool {
        matches!(
            RunState::from_u8(self.run_state.load(Ordering::Relaxed)),
            RunState::Running | RunState::RunningTriggered
        )
    }

    /// Read the next-execution-time snapshot (`None` = not periodic).
    pub fn next_exec_time(&self) -> Option<FrameworkTime> {
        let t = self.next_exec_time.load(Ordering::Acquire);
        (t != FrameworkTime::INVALID).then_some(t)
    }

    /// Overwrite the next-execution-time snapshot.
    pub fn set_next_exec_time(&self, time: Option<FrameworkTime>) {
        self.next_exec_time
            .store(time.unwrap_or(FrameworkTime::INVALID), Ordering::Release);
    }

    /// Recompute and store the next-execution-time snapshot. Only valid while
    /// the caller holds the node.
    fn refresh_next_exec_time(&self, now: FrameworkTime) {
        // SAFETY: the caller holds the node (it is `Running`).
        let next = unsafe { (*self.node.get()).next_requested_execution_time(now) };
        self.set_next_exec_time(next);
    }

    /// Claim the node, run `f`, refresh the next-execution-time snapshot, and
    /// release. Returns `(f's result, reenqueue)` — `reenqueue` is `true` if a
    /// trigger arrived mid-run; the caller must then send the node's index
    /// back to its pool's work channel (see [`release_running`](Self::release_running)).
    pub fn execute<R>(
        &self,
        now: FrameworkTime,
        f: impl FnOnce(&mut CallbackNode) -> R,
    ) -> (R, bool) {
        self.acquire_running();
        let mut release = RunningRelease {
            node: self,
            active: true,
        };
        // SAFETY: acquire_running succeeded on this thread; the reference
        // cannot outlive this call.
        let result = f(unsafe { self.node_mut_unchecked() });
        self.refresh_next_exec_time(now);
        let reenqueue = self.release_running();
        release.active = false;
        (result, reenqueue)
    }

    /// CAS `Idle → Running`. Returns `false` if the node is running or
    /// enqueued: an enqueued node's index is in a work channel, and claiming
    /// the node would let a later trigger queue a duplicate index.
    fn try_claim_idle(&self) -> bool {
        self.run_state
            .compare_exchange(IDLE, RUNNING, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Run `f` on the node. Only valid on an idle node (not running, not
    /// enqueued) — for coordinating threads at setup/teardown. Panics
    /// otherwise, or if a trigger fires during `f` (there is no enqueuer to
    /// deliver the re-run).
    pub fn access<R>(&self, f: impl FnOnce(&mut CallbackNode) -> R) -> R {
        if !self.try_claim_idle() {
            let state = RunState::from_u8(self.run_state.load(Ordering::Relaxed));
            panic!(
                "callback node is not idle ({state:?}); cannot access a running or enqueued node"
            );
        }
        let mut release = RunningRelease {
            node: self,
            active: true,
        };
        // SAFETY: try_claim_idle succeeded on this thread; the reference
        // cannot outlive this call.
        let result = f(unsafe { self.node_mut_unchecked() });
        let reenqueue = self.release_running();
        release.active = false;
        if reenqueue {
            self.run_state.store(IDLE, Ordering::Release);
        }
        assert!(
            !reenqueue,
            "callback node triggered during access; protocol violation"
        );
        result
    }

    /// Like [`access`](Self::access), but returns `None` when the node is not
    /// idle. Cleanup and diagnostics paths use this to skip busy nodes.
    pub fn try_access<R>(&self, f: impl FnOnce(&mut CallbackNode) -> R) -> Option<R> {
        if !self.try_claim_idle() {
            return None;
        }
        let mut release = RunningRelease {
            node: self,
            active: true,
        };
        // SAFETY: try_claim_idle succeeded on this thread; the reference
        // cannot outlive this call.
        let result = f(unsafe { self.node_mut_unchecked() });
        let reenqueue = self.release_running();
        release.active = false;
        if reenqueue {
            self.run_state.store(IDLE, Ordering::Release);
        }
        assert!(
            !reenqueue,
            "callback node triggered during access; protocol violation"
        );
        Some(result)
    }

    /// # Safety
    /// No thread may access this node while cleanup runs. The caller must have
    /// exclusive access to the callback interior for the duration of the call.
    pub unsafe fn cleanup_subscribers_when_quiescent(&self) {
        // SAFETY: callers guarantee no worker or scheduler can access nodes.
        unsafe {
            (*self.node.get())
                .callback()
                .for_each_subscriber(&mut |s| s.cleanup_buffers())
        };
    }

    /// SAFETY: the caller must hold the node (`run_state` is `Running` on
    /// this thread via an acquire), and the reference must not outlive the
    /// `release_running` call.
    #[expect(
        clippy::mut_from_ref,
        reason = "handing &mut out of &self is the point of the UnsafeCell interior; \
                  exclusivity is enforced by the run-state protocol, not by Rust's \
                  aliasing rules"
    )]
    unsafe fn node_mut_unchecked(&self) -> &mut CallbackNode {
        // SAFETY: the caller upholds the contract above, so the interior is
        // exclusively owned by this thread for the lifetime of the reference.
        unsafe { &mut *self.node.get() }
    }
}

/// One authoritative collection of [`SharedCallbackNode`]s backing an
/// executor.
///
/// The storage lives on the executor's coordinating thread. Worker threads
/// hold their own handle clones (via [`clone_shared`](Self::clone_shared))
/// and access nodes by index; node access is governed by each node's
/// run-state protocol. Executors must join their worker threads before
/// dropping the storage so no node is running while
/// [`cleanup_subscribers`](Self::cleanup_subscribers) runs.
#[derive(Debug)]
pub struct CallbackStorage {
    nodes: Vec<Arc<SharedCallbackNode>>,
}

/// A worker thread's private view of a [`CallbackStorage`]: a `Vec` of shared
/// node handles that can be moved into a spawned thread.
pub struct WorkerNodes(Vec<Arc<SharedCallbackNode>>);

impl std::ops::Deref for WorkerNodes {
    type Target = Vec<Arc<SharedCallbackNode>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for WorkerNodes {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl CallbackStorage {
    pub fn new() -> Self {
        CallbackStorage { nodes: Vec::new() }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        CallbackStorage {
            nodes: Vec::with_capacity(capacity),
        }
    }

    /// Wrap plain callback nodes (as produced by graph building) into storage,
    /// giving each node its own shared handle so worker threads can later
    /// clone them.
    pub fn from_nodes(nodes: Vec<CallbackNode>) -> Self {
        CallbackStorage {
            nodes: nodes
                .into_iter()
                .map(SharedCallbackNode::new)
                .map(Arc::new)
                .collect(),
        }
    }

    /// Take ownership of shared handles directly. Used when several pool
    /// storages are flattened into one authoritative collection.
    pub fn from_shared(nodes: Vec<Arc<SharedCallbackNode>>) -> Self {
        CallbackStorage { nodes }
    }

    pub fn push(&mut self, node: CallbackNode) -> CallbackNodeId {
        self.nodes.push(Arc::new(SharedCallbackNode::new(node)));
        CallbackNodeId(self.nodes.len() - 1)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Iterate over the shared node handles themselves.
    pub fn iter_shared(&self) -> impl Iterator<Item = &Arc<SharedCallbackNode>> {
        self.nodes.iter()
    }

    pub fn as_shared_slice(&self) -> &[Arc<SharedCallbackNode>] {
        &self.nodes
    }

    pub fn get(&self, id: CallbackNodeId) -> Option<&Arc<SharedCallbackNode>> {
        self.nodes.get(id.0)
    }

    pub fn get_by_name(&self, name: &str) -> Option<&Arc<SharedCallbackNode>> {
        self.iter_shared().find(|node| {
            node.try_access(|n| n.name() == name)
                .is_some_and(|found| found)
        })
    }

    pub fn node_id_by_name(&self, name: &str) -> Option<CallbackNodeId> {
        self.iter_shared().enumerate().find_map(|(index, node)| {
            node.try_access(|n| n.name() == name)
                .is_some_and(|found| found)
                .then_some(CallbackNodeId(index))
        })
    }

    /// Hand worker threads their own `Vec` of shared node handles; the clone
    /// can move into a spawned thread.
    pub fn clone_shared(&self) -> WorkerNodes {
        WorkerNodes(self.nodes.clone())
    }

    /// Extract the underlying shared handles.
    pub fn into_nodes(mut self) -> Vec<Arc<SharedCallbackNode>> {
        // std::mem::take avoids moving out of a type with a Drop impl.
        std::mem::take(&mut self.nodes)
    }

    /// Clear every subscriber's buffers while the nodes' publishers (and any
    /// per-worker execution-log publishers) are still alive. Subscribers hold
    /// [`ArenaPtr`]s into those publishers' arenas; leaving them in place
    /// until the nodes drop would dereference freed arena slots. Nodes that
    /// are not idle are skipped.
    ///
    /// Idempotent: clearing an already-clear buffer is a no-op, so calling
    /// this from both an executor's teardown and [`Drop`] is safe.
    ///
    /// [`ArenaPtr`]: base::arena::ArenaPtr
    pub fn cleanup_subscribers(&self) {
        for node in self.iter_shared() {
            node.try_access(|n| {
                n.callback()
                    .for_each_subscriber(&mut |s| s.cleanup_buffers());
            });
        }
    }

    /// # Safety
    /// No worker or scheduler may access any node while cleanup runs. The
    /// caller must have exclusive access to every callback interior for the
    /// duration of the call.
    pub unsafe fn cleanup_subscribers_when_quiescent(&self) {
        for node in self.iter_shared() {
            // SAFETY: upheld by this method's caller for every node.
            unsafe { node.cleanup_subscribers_when_quiescent() };
        }
    }
}

impl Default for CallbackStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Vec<CallbackNode>> for CallbackStorage {
    fn from(nodes: Vec<CallbackNode>) -> Self {
        Self::from_nodes(nodes)
    }
}

impl Index<usize> for CallbackStorage {
    type Output = Arc<SharedCallbackNode>;

    fn index(&self, index: usize) -> &Self::Output {
        &self.nodes[index]
    }
}

impl Index<CallbackNodeId> for CallbackStorage {
    type Output = Arc<SharedCallbackNode>;

    fn index(&self, index: CallbackNodeId) -> &Self::Output {
        &self.nodes[index.0]
    }
}

impl<'a> IntoIterator for &'a CallbackStorage {
    type Item = &'a Arc<SharedCallbackNode>;
    type IntoIter = std::slice::Iter<'a, Arc<SharedCallbackNode>>;

    fn into_iter(self) -> Self::IntoIter {
        self.nodes.iter()
    }
}

impl Drop for CallbackStorage {
    fn drop(&mut self) {
        self.cleanup_subscribers();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callback::{Callback, CallbackNode, Run};
    use crate::context::Context;
    use crate::generic_publisher::GenericPublisher;
    use crate::generic_subscriber::GenericSubscriber;
    use std::time::Duration;

    struct NoopCallback;

    impl Callback for NoopCallback {
        fn run(&mut self, _ctx: &Context) -> Run {
            Run::new(0)
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
        fn for_each_port_mut<'a>(&'a mut self, _f: &mut dyn FnMut(crate::callback::PortMut<'a>)) {}
    }

    fn make_node() -> SharedCallbackNode {
        SharedCallbackNode::new(CallbackNode::new_named(
            Box::new(NoopCallback),
            "TestNode".into(),
        ))
    }

    #[test]
    fn trigger_from_idle_enqueues_once() {
        let node = make_node();
        assert!(node.trigger(), "Idle → Enqueued should report enqueue");
        assert!(
            !node.trigger(),
            "second trigger while Enqueued should dedup"
        );
        node.acquire_running();
        node.release_running();
        assert!(node.trigger(), "Idle again after a full cycle");
    }

    #[test]
    fn trigger_while_running_defers_rerun() {
        let node = make_node();
        node.acquire_running();
        assert!(
            !node.trigger(),
            "trigger while Running should defer, not enqueue"
        );
        assert!(
            node.release_running(),
            "release after mid-run trigger should request re-enqueue"
        );
        assert!(!node.trigger(), "Enqueued again after release; dedup holds");
        node.acquire_running();
        assert!(
            !node.release_running(),
            "no trigger this run: plain release back to Idle"
        );
    }

    #[test]
    fn access_round_trips() {
        let node = make_node();
        let name = node.access(|n| n.name().to_string());
        assert_eq!(name, "TestNode");
        assert!(!node.is_running());
    }

    #[test]
    fn try_access_skips_running_node() {
        let node = make_node();
        node.acquire_running();
        assert!(node.try_access(|_| ()).is_none());
        node.release_running();
        assert!(
            node.try_access(|n| n.name() == "TestNode")
                .is_some_and(|ok| ok)
        );
    }

    #[test]
    fn try_access_does_not_clobber_enqueued_node() {
        let node = make_node();
        assert!(node.trigger(), "Idle → Enqueued should report enqueue");
        // The index is in the work channel: access must not steal the node,
        // or a later trigger could queue a duplicate index.
        assert!(node.try_access(|_| ()).is_none());
        node.acquire_running();
        assert!(
            !node.release_running(),
            "no mid-run trigger: plain release back to Idle"
        );
    }

    #[test]
    #[should_panic(expected = "not idle")]
    fn access_on_enqueued_panics() {
        let node = make_node();
        assert!(node.trigger());
        node.access(|_| ());
    }

    #[test]
    fn panic_paths_reset_running_state() {
        let node = make_node();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                node.execute(FrameworkTime::from_nanoseconds(0), |_| panic!("execute"));
            }))
            .is_err()
        );
        assert!(!node.is_running());
        assert!(node.trigger());

        let node = make_node();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                node.execute(FrameworkTime::from_nanoseconds(0), |_| {
                    node.trigger();
                    panic!("execute triggered");
                });
            }))
            .is_err()
        );
        assert!(node.trigger());

        let node = make_node();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                node.access(|_| panic!("access"));
            }))
            .is_err()
        );
        assert!(node.trigger());

        let node = make_node();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                node.try_access(|_| panic!("try access"));
            }))
            .is_err()
        );
        assert!(node.trigger());
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "spawns many threads and iterates 25k times; too slow under Miri"
    )]
    fn state_machine_concurrent_hammer_does_not_panic() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::thread;

        const TRIGGER_THREADS: usize = 4;
        const TRIGGER_ITERATIONS: usize = 25_000;

        let node = Arc::new(make_node());
        let done = Arc::new(AtomicBool::new(false));

        // One worker: the work channel hands a given index to at most one
        // worker, so two workers claiming the same node is not a scenario to
        // provoke here.
        let worker = {
            let node = Arc::clone(&node);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                while !done.load(AtomicOrdering::Relaxed) {
                    node.acquire_running();
                    let _reenqueue = node.release_running();
                    thread::yield_now();
                }
            })
        };

        let mut triggers = Vec::new();
        for _ in 0..TRIGGER_THREADS {
            let node = Arc::clone(&node);
            triggers.push(thread::spawn(move || {
                for _ in 0..TRIGGER_ITERATIONS {
                    let _ = node.trigger();
                    thread::yield_now();
                }
            }));
        }

        for t in triggers {
            t.join().unwrap();
        }
        done.store(true, AtomicOrdering::Relaxed);
        worker.join().unwrap();

        node.access(|_| ());
    }

    #[test]
    #[should_panic(expected = "protocol violation")]
    fn acquire_running_on_running_panics() {
        let node = make_node();
        node.acquire_running();
        node.acquire_running();
    }

    #[test]
    fn next_exec_time_snapshot() {
        let node = make_node();
        assert_eq!(node.next_exec_time(), None);
        let t = FrameworkTime::from_nanoseconds(42);
        node.set_next_exec_time(Some(t));
        assert_eq!(node.next_exec_time(), Some(t));
        node.set_next_exec_time(None);
        assert_eq!(node.next_exec_time(), None);
    }

    #[test]
    fn refresh_next_exec_time_uses_node_callback() {
        let mut inner = CallbackNode::new_named(Box::new(NoopCallback), "Periodic".into());
        inner.set_execution_time_callback(Box::new(|now| Some(now + Duration::from_nanos(5))));
        let node = SharedCallbackNode::new(inner);
        node.acquire_running();
        node.refresh_next_exec_time(FrameworkTime::from_nanoseconds(100));
        node.release_running();
        assert_eq!(
            node.next_exec_time(),
            Some(FrameworkTime::from_nanoseconds(105))
        );
    }
}
