//! Shared storage for the callback nodes of an executor.

use std::cell::{Ref, RefCell, RefMut};
use std::ops::Index;
use std::sync::Arc;

use crate::callback::CallbackNode;

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

/// One authoritative collection of [`CallbackNode`]s backing an executor.
///
/// # Ownership model
///
/// The storage lives on the executor's coordinating thread (the main thread
/// for the live executors, the step thread for the simulation executor). It
/// follows the "shared vec with thread-safe index access" idiom: worker
/// threads never touch the collection itself — they each hold their own
/// `Vec<Arc<RefCell<CallbackNode>>>` produced by
/// [`clone_shared`](Self::clone_shared), and access nodes by index through
/// those clones. The storage itself is not meant to be shared across threads;
/// only individual nodes are, one thread at a time.
///
/// # Concurrency invariant
///
/// The framework guarantees that no two threads ever run the same callback
/// concurrently. [`RefCell`] is the honest encoding of that invariant: unlike
/// a [`Mutex`], which would simply block on contention, two threads borrowing
/// the same node at the same time is a data race — undefined behavior, not a
/// recoverable lock conflict. Executors must join their worker threads before
/// dropping the storage so no node is borrowed while
/// [`cleanup_subscribers`](Self::cleanup_subscribers) runs.
///
/// [`Mutex`]: std::sync::Mutex
#[derive(Debug)]
pub struct CallbackStorage {
    nodes: Vec<Arc<RefCell<CallbackNode>>>,
}

/// SAFETY: Moving the storage between threads happens only to hand it to a
/// single worker thread that runs alone (the exact-replay thread). The
/// framework invariant documented on [`CallbackStorage`] — no two threads
/// access the same node concurrently — guarantees the `RefCell` interiors are
/// never borrowed from two threads at once, so the `!Sync` field is never
/// raced on.
unsafe impl Send for CallbackStorage {}

/// A worker thread's private view of a [`CallbackStorage`]: a `Vec` of shared
/// node handles that can be moved into a spawned thread.
///
/// `Vec<Arc<RefCell<CallbackNode>>>` is not `Send` on its own because
/// `RefCell` is not `Sync` — the compiler refuses to prove that a node won't
/// be borrowed from two threads at once. The framework invariant does prove
/// it: no two threads ever access the same callback node concurrently, so a
/// per-thread clone of the shared handles is safe to move across threads.
/// Borrowing a node from two threads at once remains undefined behavior (see
/// [`CallbackStorage`]).
pub struct WorkerNodes(Vec<Arc<RefCell<CallbackNode>>>);

/// SAFETY: Each `WorkerNodes` value lives on exactly one worker thread, and
/// the framework invariant (see [`CallbackStorage`]) guarantees no two threads
/// access the same node concurrently. The `RefCell` interiors are therefore
/// never borrowed from two threads at once.
unsafe impl Send for WorkerNodes {}

impl std::ops::Deref for WorkerNodes {
    type Target = Vec<Arc<RefCell<CallbackNode>>>;

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
    /// giving each node its own `Arc<RefCell<_>>` so worker threads can later
    /// clone the shared handles.
    pub fn from_nodes(nodes: Vec<CallbackNode>) -> Self {
        CallbackStorage {
            nodes: nodes.into_iter().map(Self::shared_node).collect(),
        }
    }

    /// Take ownership of shared handles directly. Used when several pool
    /// storages are flattened into one authoritative collection.
    pub fn from_shared(nodes: Vec<Arc<RefCell<CallbackNode>>>) -> Self {
        CallbackStorage { nodes }
    }

    pub fn push(&mut self, node: CallbackNode) -> CallbackNodeId {
        self.nodes.push(Self::shared_node(node));
        CallbackNodeId(self.nodes.len() - 1)
    }

    /// Wrap a node in the `Arc<RefCell<_>>` the storage is built on. Clippy
    /// flags this as an `Arc` over a `!Sync` type; the framework invariant
    /// (see [`CallbackStorage`]) makes it sound, and the `Send` markers on
    /// [`CallbackStorage`] and [`WorkerNodes`] are what let the handles cross
    /// thread boundaries.
    #[allow(clippy::arc_with_non_send_sync)]
    fn shared_node(node: CallbackNode) -> Arc<RefCell<CallbackNode>> {
        Arc::new(RefCell::new(node))
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<RefCell<CallbackNode>>> {
        self.nodes.iter()
    }

    /// Iterate over the nodes, yielding an immutable borrow of each. Nodes
    /// that are momentarily borrowed elsewhere (e.g. a worker mid-execution)
    /// are skipped, so call this from the coordinating thread that owns the
    /// storage, where nodes are never busy.
    pub fn iter_borrowed(&self) -> impl Iterator<Item = Ref<'_, CallbackNode>> {
        self.nodes.iter().filter_map(|node| node.try_borrow().ok())
    }

    /// Like [`iter_borrowed`](Self::iter_borrowed), yielding the node index
    /// alongside each borrow.
    pub fn iter_borrowed_enumerated(&self) -> impl Iterator<Item = (usize, Ref<'_, CallbackNode>)> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| node.try_borrow().ok().map(|node| (index, node)))
    }

    /// Like [`iter_borrowed`](Self::iter_borrowed), yielding mutable borrows.
    pub fn iter_borrowed_mut(&self) -> impl Iterator<Item = RefMut<'_, CallbackNode>> {
        self.nodes
            .iter()
            .filter_map(|node| node.try_borrow_mut().ok())
    }

    /// Like [`iter_borrowed_mut`](Self::iter_borrowed_mut), yielding the node
    /// index alongside each borrow.
    pub fn iter_borrowed_mut_enumerated(
        &self,
    ) -> impl Iterator<Item = (usize, RefMut<'_, CallbackNode>)> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| node.try_borrow_mut().ok().map(|node| (index, node)))
    }

    pub fn get(&self, id: CallbackNodeId) -> Option<&Arc<RefCell<CallbackNode>>> {
        self.nodes.get(id.0)
    }

    pub fn get_by_name(&self, name: &str) -> Option<&Arc<RefCell<CallbackNode>>> {
        self.iter().find(|node| node.borrow().name() == name)
    }

    pub fn node_id_by_name(&self, name: &str) -> Option<CallbackNodeId> {
        self.iter_borrowed_enumerated()
            .find(|(_, node)| node.name() == name)
            .map(|(index, _)| CallbackNodeId(index))
    }

    /// Borrow a node immutably, returning `None` if the node is currently
    /// borrowed (rather than panicking). Executors use this in cleanup paths
    /// where a busy node should simply be skipped.
    pub fn borrow(&self, id: CallbackNodeId) -> Option<Ref<'_, CallbackNode>> {
        self.nodes.get(id.0)?.try_borrow().ok()
    }

    /// Borrow a node mutably, returning `None` if the node is currently
    /// borrowed (rather than panicking).
    pub fn borrow_mut(&self, id: CallbackNodeId) -> Option<RefMut<'_, CallbackNode>> {
        self.nodes.get(id.0)?.try_borrow_mut().ok()
    }

    /// Hand worker threads their own `Vec` of shared node handles, wrapped in
    /// a `Send` marker so the clone can move into a spawned thread. The
    /// returned value aliases the same nodes as this storage; callers must
    /// obey the single-threaded-access invariant documented on
    /// [`CallbackStorage`].
    pub fn clone_shared(&self) -> WorkerNodes {
        WorkerNodes(self.nodes.clone())
    }

    /// Extract the underlying shared handles.
    pub fn into_nodes(mut self) -> Vec<Arc<RefCell<CallbackNode>>> {
        // std::mem::take avoids moving out of a type with a Drop impl.
        std::mem::take(&mut self.nodes)
    }

    /// Clear every subscriber's buffers while the nodes' publishers (and any
    /// per-worker execution-log publishers) are still alive. Subscribers hold
    /// [`ArenaPtr`]s into those publishers' arenas; leaving them in place
    /// until the nodes drop would dereference freed arena slots. Uses
    /// `try_borrow` so a node that is momentarily borrowed (e.g. a worker
    /// still finishing an execution) is skipped rather than panicking.
    ///
    /// Idempotent: clearing an already-clear buffer is a no-op, so calling
    /// this from both an executor's teardown and [`Drop`] is safe.
    ///
    /// [`ArenaPtr`]: base::arena::ArenaPtr
    pub fn cleanup_subscribers(&self) {
        for node in self.iter() {
            if let Ok(guard) = node.try_borrow() {
                guard.callback().for_each_subscriber(&mut |s| {
                    s.cleanup_buffers();
                });
            }
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
    type Output = Arc<RefCell<CallbackNode>>;

    fn index(&self, index: usize) -> &Self::Output {
        &self.nodes[index]
    }
}

impl Index<CallbackNodeId> for CallbackStorage {
    type Output = Arc<RefCell<CallbackNode>>;

    fn index(&self, index: CallbackNodeId) -> &Self::Output {
        &self.nodes[index.0]
    }
}

impl<'a> IntoIterator for &'a CallbackStorage {
    type Item = &'a Arc<RefCell<CallbackNode>>;
    type IntoIter = std::slice::Iter<'a, Arc<RefCell<CallbackNode>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.nodes.iter()
    }
}

impl Drop for CallbackStorage {
    fn drop(&mut self) {
        self.cleanup_subscribers();
    }
}
