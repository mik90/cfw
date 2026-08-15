//! Shared storage for the callback nodes of an executor.

use std::cell::UnsafeCell;
use std::ops::Index;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::callback::CallbackNode;
use crate::time::{AtomicFrameworkTime, FrameworkTime};

/// Strong index into a [`CallbackStorage`]. Wrap raw `usize` node indices in
/// this type so "this is a node index" is visible at the type level instead of
/// being an anonymous `usize` threading through the executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallbackNodeId(pub usize);

impl From<usize> for CallbackNodeId {
    fn from(value: usize) -> Self {
        CallbackNodeId(value)
    }
}

impl From<CallbackNodeId> for usize {
    fn from(value: CallbackNodeId) -> Self {
        value.0
    }
}

/// The scheduling state of a [`SharedCallbackNode`], enforced atomically.
///
/// The state machine:
///
/// ```text
///            trigger                worker dequeues
///  Idle ───────────────▶ Enqueued ─────────────────▶ Running
///   ▲                        │                          │ │
///   │                        │                          │ │ worker done
///   │                        │ re-enqueue (after        │ │ (no trigger fired)
///   │                        │ a trigger during run)    │ │
///   │                        │      ◀──────────────────┘ │
///   └────────────────────────┘          trigger          │
///                                         ──────────────▶│ RunningTriggered
/// ```
///
/// - `trigger` moves `Idle → Enqueued` (the caller must then send the node
///   index to its pool's work channel) or `Running → RunningTriggered`
///   (remembering that the node must run again once the current run ends).
///   It is a no-op from `Enqueued` / `RunningTriggered` — deduplication, so a
///   node is never in a pool's work channel twice.
/// - A worker moves `Enqueued → Running` before touching the node and back to
///   `Idle` (or `Enqueued` if a trigger arrived mid-run) after.
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
            _ => RunState::RunningTriggered,
        }
    }
}

const IDLE: u8 = RunState::Idle as u8;
const ENQUEUED: u8 = RunState::Enqueued as u8;
const RUNNING: u8 = RunState::Running as u8;
const RUNNING_TRIGGERED: u8 = RunState::RunningTriggered as u8;

/// A [`CallbackNode`] shared between an executor's coordinating thread and
/// its worker threads, guarded by an atomic run-state protocol instead of a
/// lock.
///
/// # Concurrency protocol
///
/// Exclusive access to the wrapped node is enforced by [`RunState`]: only the
/// thread that successfully transitions the node into `Running` may touch the
/// [`UnsafeCell`] interior, and only until it transitions the state back out.
/// This replaces the old `Arc<RefCell<_>>` arrangement whose single-access
/// guarantee rested on executor convention — and whose cross-thread failure
/// mode was a data race on the `RefCell` borrow flag, i.e. silent undefined
/// behavior. Here a protocol violation is either impossible (a second worker
/// cannot win the same CAS) or panics loudly (a quiescent accessor finding a
/// non-`Idle` node via `with_exclusive`).
///
/// The next-execution-time snapshot (`next_exec_time`) exists so scheduling
/// threads (e.g. the live executor's periodic thread) never have to read node
/// internals across threads: the worker refreshes the snapshot after each run
/// while it still owns the node.
#[derive(Debug)]
pub struct SharedCallbackNode {
    run_state: AtomicU8,
    next_exec_time: AtomicFrameworkTime,
    node: UnsafeCell<CallbackNode>,
}

/// SAFETY: The `UnsafeCell` interior is only ever accessed by the thread that
/// won the atomic `run_state` CAS into `Running` (see the protocol on
/// [`SharedCallbackNode`]), so `&mut` access is never shared across threads.
/// The remaining fields are atomics. `CallbackNode` is itself `Send + Sync`
/// (its callbacks may run on any thread), which covers handing the node
/// contents between threads across sequential runs.
unsafe impl Sync for SharedCallbackNode {}

impl SharedCallbackNode {
    pub fn new(node: CallbackNode) -> Self {
        SharedCallbackNode {
            run_state: AtomicU8::new(RunState::Idle as u8),
            next_exec_time: AtomicFrameworkTime::new(FrameworkTime::INVALID),
            node: UnsafeCell::new(node),
        }
    }

    /// Request an execution of this node.
    ///
    /// Returns `true` if the node transitioned `Idle → Enqueued`: the caller
    /// must now send the node's index to its pool's work channel. Returns
    /// `false` if the node is already enqueued (`Enqueued` / `RunningTriggered`
    /// — deduplicated) or currently running (`Running → RunningTriggered` —
    /// the re-run is remembered and happens when the current run ends).
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
                    // The worker released (or another trigger deferred) between
                    // our two CASes; loop and re-evaluate from the new state.
                }
                Err(ENQUEUED) | Err(RUNNING_TRIGGERED) => return false,
                Err(other) => unreachable!("run_state held invalid value {other}"),
            }
        }
    }

    /// Claim exclusive access to the node for execution. The caller must have
    /// received this node's index from a pool work channel (i.e. the node is
    /// `Enqueued`), or be a quiescent coordinating context (node is `Idle`).
    ///
    /// Panics if the node is already `Running` / `RunningTriggered` — that is
    /// a scheduling protocol violation (today's silent data race).
    pub fn acquire_running(&self) {
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
                    // Someone else raced us out of Idle (a trigger enqueued
                    // the node, or another accessor claimed it); loop and
                    // retry against the new state.
                }
                Err(RUNNING) | Err(RUNNING_TRIGGERED) => {
                    panic!(
                        "callback node is already running; concurrent execution is a protocol violation"
                    )
                }
                Err(other) => unreachable!("run_state held invalid value {other}"),
            }
        }
    }

    /// Release the node after a run. Returns `true` when a trigger arrived
    /// mid-run (`RunningTriggered → Enqueued`): the caller must send the
    /// node's index back to its pool's work channel. Returns `false` for the
    /// plain `Running → Idle` transition.
    pub fn release_running(&self) -> bool {
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
                    // A trigger landed between our two CASes (`Running →
                    // RunningTriggered`); loop and the next iteration takes the
                    // RunningTriggered arm and reports the deferred re-run.
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

    /// Recompute and store the next-execution-time snapshot from the node's
    /// execution-time callback. Only meaningful while the caller holds the
    /// node (between `acquire_running` and `release_running`); computing it
    /// reads node internals.
    pub fn refresh_next_exec_time(&self, now: FrameworkTime) {
        // SAFETY: callers must currently hold the node (it is `Running`),
        // enforced by the run_state protocol.
        let next = unsafe { (*self.node.get()).next_requested_execution_time(now) };
        self.set_next_exec_time(next);
    }

    /// Worker-style execution: claim the node, run `f`, refresh the
    /// next-execution-time snapshot while still holding it, then release.
    ///
    /// Returns `(f's result, reenqueue)`: `reenqueue` is `true` when a
    /// trigger arrived mid-run — the caller must then send the node's index
    /// back to its pool's work channel (the node is already `Enqueued`
    /// again; this only feeds the queue).
    pub fn execute<R>(
        &self,
        now: FrameworkTime,
        f: impl FnOnce(&mut CallbackNode) -> R,
    ) -> (R, bool) {
        self.acquire_running();
        // SAFETY: acquire_running succeeded on this thread, so we hold the
        // node; the reference cannot outlive this call (it ends before
        // release_running).
        let result = f(unsafe { self.node_mut_unchecked() });
        self.refresh_next_exec_time(now);
        let reenqueue = self.release_running();
        (result, reenqueue)
    }

    /// Quiescent acquire: only valid from `Idle`. Returns `false` when the
    /// node is anything but `Idle` — running, or enqueued with its index in a
    /// pool work channel. A quiescent accessor must not touch an enqueued
    /// node: the work channel still holds its index, and stealing the node
    /// (`Enqueued → Running → Idle`) would let a later `trigger` enqueue a
    /// second index, so the node would run twice when both are dequeued.
    fn acquire_quiescent(&self) -> bool {
        self.run_state
            .compare_exchange(IDLE, RUNNING, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Run `f` with exclusive access to the node, acquiring it first and
    /// releasing it after. For quiescent contexts (build time, diagnostics on
    /// a stopped executor) that require the node to be `Idle`.
    ///
    /// Panics if the node is not `Idle` (busy or enqueued — see
    /// [`acquire_quiescent`](Self::acquire_quiescent)), or if a trigger fires
    /// during `f` — quiescent contexts have no enqueuer wired up, so a
    /// deferred re-run could never be delivered. A trigger during a worker's
    /// run is handled by `process_work_item` explicitly.
    pub fn with_exclusive<R>(&self, f: impl FnOnce(&mut CallbackNode) -> R) -> R {
        if !self.acquire_quiescent() {
            let state = RunState::from_u8(self.run_state.load(Ordering::Relaxed));
            panic!(
                "callback node is not idle ({state:?}); quiescent exclusive access is a protocol violation"
            );
        }
        // SAFETY: acquire_quiescent succeeded (Idle → Running), so we hold the
        // node; the reference cannot outlive this call (before
        // release_running).
        let result = f(unsafe { self.node_mut_unchecked() });
        let reenqueue = self.release_running();
        assert!(
            !reenqueue,
            "callback node triggered during exclusive access with no enqueuer; protocol violation"
        );
        result
    }

    /// Like [`with_exclusive`](Self::with_exclusive), but returns `None`
    /// instead of panicking when the node is not `Idle` — running (a worker
    /// mid-execution) or enqueued (its index already in a pool work channel;
    /// touching it would desynchronize the queue and allow a duplicate
    /// enqueue). Diagnostics and cleanup paths use this to skip non-idle
    /// nodes.
    pub fn try_with_exclusive<R>(&self, f: impl FnOnce(&mut CallbackNode) -> R) -> Option<R> {
        if !self.acquire_quiescent() {
            return None;
        }
        // SAFETY: acquire_quiescent succeeded (Idle → Running), so we hold the
        // node; the reference cannot outlive this call (before
        // release_running).
        let result = f(unsafe { self.node_mut_unchecked() });
        let reenqueue = self.release_running();
        assert!(
            !reenqueue,
            "callback node triggered during exclusive access with no enqueuer; protocol violation"
        );
        Some(result)
    }

    /// SAFETY: caller must hold the node: `run_state` must be `Running` on
    /// this thread (via `acquire_running` or `acquire_quiescent`), and the
    /// reference must not outlive the `release_running` call.
    #[expect(
        clippy::mut_from_ref,
        reason = "handing &mut out of &self is the point of the UnsafeCell interior; \
                  exclusivity is enforced by the run-state protocol, not by Rust's \
                  aliasing rules"
    )]
    unsafe fn node_mut_unchecked(&self) -> &mut CallbackNode {
        // SAFETY: the caller upholds the contract above (the node is held via
        // an acquire and the returned reference does not escape the enclosing
        // run), so the `UnsafeCell` interior is exclusively owned by this
        // thread for the lifetime of the returned reference.
        unsafe { &mut *self.node.get() }
    }
}

/// One authoritative collection of [`SharedCallbackNode`]s backing an
/// executor.
///
/// # Ownership model
///
/// The storage lives on the executor's coordinating thread (the main thread
/// for the live executors, the step thread for the simulation executor).
/// Worker threads never touch the collection itself — they each hold their
/// own `Vec<Arc<SharedCallbackNode>>` produced by
/// [`clone_shared`](Self::clone_shared), and access nodes by index through
/// those clones.
///
/// # Concurrency
///
/// Node access is governed by each node's atomic run-state protocol (see
/// [`SharedCallbackNode`]) — no locks, and single-threaded access is enforced
/// by the state machine rather than by executor convention. Executors must
/// still join their worker threads before dropping the storage so no node is
/// running while [`cleanup_subscribers`](Self::cleanup_subscribers) runs.
#[derive(Debug)]
pub struct CallbackStorage {
    nodes: Vec<Arc<SharedCallbackNode>>,
}

/// A worker thread's private view of a [`CallbackStorage`]: a `Vec` of shared
/// node handles that can be moved into a spawned thread. Aliases the same
/// nodes as the storage; node access remains governed by the
/// [`SharedCallbackNode`] run-state protocol.
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

    pub fn get(&self, id: CallbackNodeId) -> Option<&Arc<SharedCallbackNode>> {
        self.nodes.get(id.0)
    }

    pub fn get_by_name(&self, name: &str) -> Option<&Arc<SharedCallbackNode>> {
        self.iter_shared().find(|node| {
            node.try_with_exclusive(|n| n.name() == name)
                .is_some_and(|found| found)
        })
    }

    pub fn node_id_by_name(&self, name: &str) -> Option<CallbackNodeId> {
        self.iter_shared().enumerate().find_map(|(index, node)| {
            node.try_with_exclusive(|n| n.name() == name)
                .is_some_and(|found| found)
                .then_some(CallbackNodeId(index))
        })
    }

    /// Hand worker threads their own `Vec` of shared node handles; the clone
    /// can move into a spawned thread. The returned value aliases the same
    /// nodes as this storage; node access remains governed by the
    /// [`SharedCallbackNode`] run-state protocol.
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
    /// are not `Idle` (running, or enqueued with their index in a work
    /// channel) are skipped rather than panicked on — stealing an enqueued
    /// node would desynchronize the work queue.
    ///
    /// Idempotent: clearing an already-clear buffer is a no-op, so calling
    /// this from both an executor's teardown and [`Drop`] is safe.
    ///
    /// [`ArenaPtr`]: base::arena::ArenaPtr
    pub fn cleanup_subscribers(&self) {
        for node in self.iter_shared() {
            node.try_with_exclusive(|n| {
                n.callback().for_each_subscriber(&mut |s| {
                    s.cleanup_buffers();
                });
            });
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
    fn with_exclusive_round_trips() {
        let node = make_node();
        let name = node.with_exclusive(|n| n.name().to_string());
        assert_eq!(name, "TestNode");
        assert!(!node.is_running());
    }

    #[test]
    fn try_with_exclusive_skips_running_node() {
        let node = make_node();
        node.acquire_running();
        assert!(node.try_with_exclusive(|_| ()).is_none());
        node.release_running();
        assert!(
            node.try_with_exclusive(|n| n.name() == "TestNode")
                .is_some_and(|ok| ok)
        );
    }

    #[test]
    fn try_with_exclusive_does_not_clobber_enqueued_node() {
        let node = make_node();
        assert!(node.trigger(), "Idle → Enqueued should report enqueue");
        // The node's index is now in its pool's work channel. A quiescent
        // accessor must not steal it: flipping Enqueued → Running → Idle would
        // leave a stale index in the channel and let a later trigger enqueue a
        // second one, running the node twice.
        assert!(node.try_with_exclusive(|_| ()).is_none());
        // The node is still Enqueued, so a worker can claim it normally.
        node.acquire_running();
        assert!(
            !node.release_running(),
            "no mid-run trigger: plain release back to Idle"
        );
    }

    #[test]
    #[should_panic(expected = "quiescent exclusive access")]
    fn with_exclusive_on_enqueued_panics() {
        let node = make_node();
        assert!(node.trigger());
        node.with_exclusive(|_| ());
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn state_machine_concurrent_hammer_does_not_panic() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::thread;

        const TRIGGER_THREADS: usize = 4;
        const WORKER_THREADS: usize = 1;
        const TRIGGER_ITERATIONS: usize = 25_000;

        let node = Arc::new(make_node());
        let done = Arc::new(AtomicBool::new(false));

        // One worker thread models the executor invariant that a node is only
        // ever run by one worker at a time (the dedup'd work channel hands a
        // given index to at most one worker). Two workers hammering
        // `acquire_running` on the *same* node would legitimately collide on
        // the Idle state — a real protocol violation, not something the stress
        // test should provoke.
        let mut workers = Vec::new();
        for _ in 0..WORKER_THREADS {
            let node = Arc::clone(&node);
            let done = Arc::clone(&done);
            workers.push(thread::spawn(move || {
                while !done.load(AtomicOrdering::Relaxed) {
                    // Worker-style claim/no-op/release. A mid-run trigger just
                    // re-enqueues (reenqueue=true); a real worker would feed
                    // the channel — here the loop simply keeps cycling.
                    node.acquire_running();
                    let _reenqueue = node.release_running();
                    thread::yield_now();
                }
            }));
        }

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
        for w in workers {
            w.join().unwrap();
        }

        // Every trigger has finished and the workers have drained the node
        // back to Idle; quiescent access must succeed.
        node.with_exclusive(|_| ());
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
