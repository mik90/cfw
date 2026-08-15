use task::callback_storage::{CallbackStorage, SharedCallbackNode};
use task::executor::ThreadPoolConfig;

use crate::node_executor::{NodeExecutionRequest, NodeExecutionResponse, node_executor_thread};
use crate::{
    CallbackNodeIndex, FrameworkTime, PoolIndex, SimulationConfig, TimeTriggeredNode, VirtualPool,
};

#[derive(Debug)]
pub enum StepError {
    /// No node executor threads are configured.
    NoNodeExecutors,
    /// A node executor thread disconnected, likely because it panicked.
    NodeExecutorThreadDisconnected,
    /// Received more responses than expected from node executor threads.
    UnexpectedResponse,
    /// The step loop thread panicked.
    StepThreadPanicked,
}
use std::collections::VecDeque;
use std::num::Saturating;
use std::sync::Arc;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

pub struct SimulationState {
    /// Storage of all callback nodes. A node's index into this vec is used to index into other Vecs.
    /// Each node is guarded by the atomic run-state protocol of
    /// [`SharedCallbackNode`] so the parallel node executors can run individual
    /// nodes without unsafe disjoint-index tricks. The framework invariant — no
    /// two node executor threads run the same node concurrently — is now
    /// enforced by the protocol rather than by `RefCell` borrowing convention.
    nodes: CallbackStorage,

    /// Threads that execute callback nodes in parallel.
    node_executor_threads: Vec<thread::JoinHandle<()>>,

    /// Tells node executors to do work
    node_exec_request_senders: Vec<Sender<NodeExecutionRequest>>,

    /// Mono channel with all results of execution
    node_exec_response_receiver: Receiver<NodeExecutionResponse>,

    /// Maps each global callback node index to its pool index
    node_to_pool: Vec<PoolIndex>,

    /// Virtual pools — no real threads, but models concurrency boundaries
    virtual_pools: Vec<VirtualPool>,

    /// TODO should this be a sorted queue?
    periodic_nodes: VecDeque<TimeTriggeredNode>,

    /// Per-node sim-time when each node's last execution finishes. Initialized to start_time.
    node_busy_until: Vec<FrameworkTime>,

    /// Whether each node currently holds a virtual thread from its pool. Set
    /// when the node is allocated a thread, cleared once its busy period has
    /// elapsed — a node can only be freed if it actually occupied a thread.
    node_thread_occupied: Vec<bool>,

    /// Sim-time when each node first became ready but hadn't yet been allocated a thread.
    /// None if the node is not currently waiting. Used to prioritize longest-waiting nodes.
    node_ready_since: Vec<Option<FrameworkTime>>,

    /// Current simulation time
    time: FrameworkTime,

    /// Number of times the state has been stepped
    step_count: Saturating<usize>,
}

impl SimulationState {
    /// Create a single virtual pool with `num_virtual_threads` for all callback nodes,
    /// starting at simulation time zero
    pub fn new(num_virtual_threads: usize, nodes: impl Into<CallbackStorage>) -> Self {
        Self::new_with(SimulationConfig {
            start_time: FrameworkTime::from_nanoseconds(0),
            pools: vec![ThreadPoolConfig::new(num_virtual_threads, nodes)],
            node_executor_thread_count: 1,
        })
    }

    /// Create an new state from a [`SimulationConfig`], supporting multiple virtual
    /// pools and a configurable start time.
    pub fn new_with(config: SimulationConfig) -> SimulationState {
        let mut all_nodes: Vec<Arc<SharedCallbackNode>> = vec![];
        let mut node_to_pool: Vec<usize> = Vec::new();
        let mut virtual_pools: Vec<VirtualPool> = Vec::new();

        for (pool_idx, pool) in config.pools.into_iter().enumerate() {
            virtual_pools.push(VirtualPool {
                virtual_thread_count: pool.thread_count,
                num_threads_occupied: 0,
            });
            for node in pool.nodes.into_nodes() {
                node_to_pool.push(pool_idx);
                all_nodes.push(node);
            }
        }

        let num_nodes = all_nodes.len();

        // One response channel shared by all node executor threads
        let (exec_response_sender, exec_response_recv): (
            Sender<NodeExecutionResponse>,
            Receiver<NodeExecutionResponse>,
        ) = mpsc::channel();

        let mut state = SimulationState {
            nodes: CallbackStorage::from_shared(all_nodes),
            node_executor_threads: Vec::with_capacity(config.node_executor_thread_count),
            node_exec_request_senders: Vec::with_capacity(config.node_executor_thread_count),
            node_exec_response_receiver: exec_response_recv,
            virtual_pools,
            node_to_pool,
            periodic_nodes: VecDeque::new(),
            node_busy_until: vec![config.start_time; num_nodes],
            node_thread_occupied: vec![false; num_nodes],
            node_ready_since: vec![None; num_nodes],
            time: config.start_time,
            step_count: Saturating(0),
        };
        for _ in 0..config.node_executor_thread_count {
            // Each thread has its own request receiver; the state owns the matching sender
            let (request_sender, request_recv): (
                Sender<NodeExecutionRequest>,
                Receiver<NodeExecutionRequest>,
            ) = mpsc::channel();
            state.node_exec_request_senders.push(request_sender);

            let cloned_nodes = state.nodes.clone_shared();

            let response_sender_clone = exec_response_sender.clone();
            state.node_executor_threads.push(thread::spawn(move || {
                node_executor_thread(request_recv, response_sender_clone, cloned_nodes);
            }));
        }

        state
    }

    pub fn start(&mut self) {
        // Set up periodic execution
        for (index, node) in self.nodes.iter_shared().enumerate() {
            // Quiescent at startup (no work has been dispatched yet), but
            // prefer try_ to skip a node that is somehow already running.
            let is_periodic = node
                .try_access(|n| n.next_requested_execution_time(self.time).is_some())
                .unwrap_or(false);
            if is_periodic {
                self.periodic_nodes.push_back(TimeTriggeredNode {
                    index,
                    // Periodic nodes will run on startup, and then their requested times will be honored
                    requested_exec_time: self.time,
                });
            }
        }
    }

    /// Finds callback nodes that should run this step, allocates a thread from their pool to each,
    /// and drains their subscribers (write → read) so data is available when they run.
    ///
    /// A node is a candidate if it has new trigger data in its write buffer and isn't busy,
    /// or it is a periodic node that is due and isn't busy. Among candidates, only those
    /// whose pool has a free thread are returned. Drain is deferred until after thread
    /// allocation so that nodes which can't run don't consume their trigger data.
    fn allocate_nodes_to_threads(&mut self) -> Vec<CallbackNodeIndex> {
        let mut candidates: Vec<CallbackNodeIndex> = vec![];

        for (index, node) in self.nodes.iter_shared().enumerate() {
            // A trigger fired and every required input has a value — the node
            // can actually run. Without the required-input check, a node with
            // a required non-trigger input would run while that input is
            // still empty.
            let ready = node
                .try_access(|n| {
                    n.subscribers_request_execution()
                        && n.required_inputs_ready()
                        && self.time >= self.node_busy_until[index]
                })
                .unwrap_or(false);
            if ready {
                candidates.push(index);
            }
        }
        for periodic in &self.periodic_nodes {
            if periodic.requested_exec_time <= self.time
                && self.time >= self.node_busy_until[periodic.index]
                && !candidates.contains(&periodic.index)
            {
                candidates.push(periodic.index);
            }
        }

        // Record when each node first became ready, then sort by wait time (oldest first)
        // with node index as a tiebreaker to preserve determinism.
        for &index in &candidates {
            self.node_ready_since[index].get_or_insert(self.time);
        }
        candidates.sort_by_key(|&index| (self.node_ready_since[index], index));

        let mut runnable: Vec<CallbackNodeIndex> = vec![];
        for index in candidates {
            let pool_index = self.node_to_pool[index];
            let pool = &mut self.virtual_pools[pool_index];
            if pool.num_threads_occupied < pool.virtual_thread_count {
                pool.num_threads_occupied += 1;
                self.node_thread_occupied[index] = true;
                self.node_ready_since[index] = None;
                runnable.push(index);
            }
        }

        runnable
    }

    pub fn step(&mut self) -> Result<Vec<CallbackNodeIndex>, StepError> {
        let runnable_nodes = self.allocate_nodes_to_threads();
        // Only drain subscribers for nodes that actually got a thread, so that nodes
        // blocked by pool pressure keep their trigger data for the next step.
        for &index in &runnable_nodes {
            self.nodes[index].access(|n| n.drain_subscribers());
        }

        let time = self.time;

        let mut sender_cycle_iter = self.node_exec_request_senders.iter().cycle();
        for index in &runnable_nodes {
            // Round-robin work across node executor threads.
            sender_cycle_iter
                .next()
                .ok_or(StepError::NoNodeExecutors)?
                .send(NodeExecutionRequest {
                    index: *index,
                    current_time: time,
                    should_run: true,
                })
                .map_err(|_| StepError::NodeExecutorThreadDisconnected)?;
        }

        let mut execution_responses: Vec<NodeExecutionResponse> = vec![];
        for _ in &runnable_nodes {
            let response = self
                .node_exec_response_receiver
                .recv()
                .map_err(|_| StepError::NodeExecutorThreadDisconnected)?;
            execution_responses.push(response);
        }

        if self.node_exec_response_receiver.try_recv().is_ok() {
            return Err(StepError::UnexpectedResponse);
        }

        for response in execution_responses {
            self.node_busy_until[response.index] = time + response.execution_duration;
        }

        for &index in &runnable_nodes {
            self.nodes[index].access(|n| n.flush_publishers(time));
        }

        // Update periodic node next-run times from their no-longer-busy instant
        for periodic in &mut self.periodic_nodes {
            if runnable_nodes.contains(&periodic.index) {
                let no_longer_busy = self.node_busy_until[periodic.index];
                if let Some(next_time) = self.nodes[periodic.index]
                    .access(|n| n.next_requested_execution_time(no_longer_busy))
                {
                    periodic.requested_exec_time = next_time;
                }
            }
        }

        // Advance sim time to earliest next event. Times not strictly in the
        // future are excluded: a zero-duration node's busy_until equals the
        // current time (it's already free), so it must not win the min
        // against the next periodic event and freeze the clock.
        let next_busy = runnable_nodes
            .iter()
            .map(|&i| self.node_busy_until[i])
            .filter(|&t| t > self.time)
            .min();
        let next_periodic = self
            .periodic_nodes
            .iter()
            .map(|p| p.requested_exec_time)
            .filter(|&t| t > self.time)
            .min();
        if let Some(t) = [next_busy, next_periodic].into_iter().flatten().min()
            && t > self.time
        {
            self.time = t;
        }

        // Free the virtual thread of any node whose busy period has elapsed.
        // Zero-duration nodes finish the instant they start (busy_until <=
        // current time), so they free their thread right away — comparing
        // against the pre-advance time instead would never free them and
        // would wedge their pool's thread permanently.
        for index in 0..self.nodes.len() {
            if self.node_thread_occupied[index] && self.node_busy_until[index] <= self.time {
                self.node_thread_occupied[index] = false;
                let pool_index = self.node_to_pool[index];
                self.virtual_pools[pool_index].num_threads_occupied -= 1;
            }
        }
        self.step_count += 1;
        Ok(runnable_nodes)
    }

    pub fn shutdown_node_executor_threads(&mut self) -> Result<(), Vec<usize>> {
        for sender in self.node_exec_request_senders.drain(..) {
            // Best-effort: thread may already have exited if its sender was dropped.
            let _ = sender.send(NodeExecutionRequest {
                index: 0,
                current_time: FrameworkTime::INVALID,
                should_run: false,
            });
        }

        let mut panicked_thread_indexes = vec![];
        for (thread_idx, t) in self.node_executor_threads.drain(..).enumerate() {
            if t.join().is_err() {
                panicked_thread_indexes.push(thread_idx);
            }
        }

        if panicked_thread_indexes.is_empty() {
            Ok(())
        } else {
            Err(panicked_thread_indexes)
        }
    }

    pub fn step_count(&self) -> Saturating<usize> {
        self.step_count
    }

    pub fn simulation_time(&self) -> FrameworkTime {
        self.time
    }

    pub fn cleanup(&mut self) {
        self.nodes.cleanup_subscribers();
    }
}

impl Drop for SimulationState {
    fn drop(&mut self) {
        // Make sure node executor threads exit even if stop() was never called.
        let _ = self.shutdown_node_executor_threads();
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use test_tasks::*;

    use super::SimulationState;

    /// Lower level test that manually steps sim state
    #[test]
    fn test_simulation_state() {
        let (nodes, task_info) = build_fizz_buzz_callback_nodes();

        let mut state = SimulationState::new(1, nodes);
        state.start();
        let start_time = state.simulation_time();

        assert_eq!(state.nodes.len(), 3);

        let periodic = state.periodic_nodes.front().unwrap();
        assert_eq!(periodic.index, 0);
        assert_eq!(periodic.requested_exec_time, start_time);

        let executed_nodes = state.step().unwrap();
        assert_eq!(executed_nodes, vec![task_info.integer_publisher_index]);

        // After first step, the publisher should want to run in the future
        let periodic = state.periodic_nodes.front().unwrap();
        assert_eq!(periodic.index, 0);
        assert_eq!(
            periodic.requested_exec_time,
            start_time + Duration::from_millis(1) + Duration::from_millis(500),
            "Publisher callback node takes 1ms to run and wants to run every 500ms"
        );

        let executed_nodes = state.step().unwrap();
        assert_eq!(
            executed_nodes,
            vec![task_info.fizz_buzz_index],
            "After the second step, the fizz-buzz callback node should have run"
        );

        let executed_nodes = state.step().unwrap();
        assert_eq!(
            executed_nodes,
            vec![task_info.string_store_index],
            "After the third step, the string store callback node should've run"
        );

        assert_eq!(task_info.stored_strings(), vec!["FizzBuzz"]);
    }

    /// Regression test: a zero-duration node must free its pool's virtual
    /// thread as soon as its step completes. Previously the thread-freeing
    /// check required `busy_until > old_sim_time`, which a zero-duration run
    /// never satisfies — the node's first execution wedged its pool thread
    /// permanently and, with a single-threaded pool, froze the whole
    /// simulation (this is the shape `LogTask` uses, so logging deadlocked
    /// the executor after one drain).
    #[test]
    fn test_zero_duration_node_frees_pool_thread() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use task::callback::{Callback, PortMut, Run};
        use task::context::Context;
        use task::generic_publisher::GenericPublisher;
        use task::generic_subscriber::GenericSubscriber;

        struct CountingCallback(Arc<AtomicUsize>);
        impl Callback for CountingCallback {
            fn run(&mut self, _ctx: &Context) -> Run {
                self.0.fetch_add(1, Ordering::Relaxed);
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

        let run_count = Arc::new(AtomicUsize::new(0));
        let mut node = task::callback::CallbackNode::new_named(
            Box::new(CountingCallback(run_count.clone())),
            "zero_duration".into(),
        );
        node.set_execution_duration_callback(Box::new(|| Duration::ZERO));
        node.set_execution_time_callback(Box::new(|now| Some(now + Duration::from_nanos(1))));

        // Single-threaded pool: if the zero-duration node never frees its
        // thread, it runs at most once.
        let mut state = SimulationState::new(1, vec![node]);
        state.start();

        for _ in 0..5 {
            let executed = state.step().unwrap();
            assert_eq!(executed, vec![0], "node should run every step");
        }
        assert_eq!(
            run_count.load(Ordering::Relaxed),
            5,
            "zero-duration node must run once per step, not wedge the pool"
        );
    }

    /// A node with a required trigger input and a required non-trigger input
    /// must only run once BOTH have values — and must re-run on later trigger
    /// data while the non-trigger input's value is retained.
    #[test]
    fn test_required_non_trigger_input_gates_execution() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use task::callback::{Callback, PortMut, Run};
        use task::context::Context;
        use task::generic_publisher::GenericPublisher;
        use task::generic_subscriber::GenericSubscriber;
        use task::output::Output;
        use task::publisher::{Publisher, PublisherConfig};
        use task::subscriber::{Subscriber, SubscriberConfig};

        /// Publishes an incrementing counter, starting at `start`, once per
        /// run, up to `max` messages.
        struct CounterPublisher {
            publisher: Publisher<u64>,
            next: u64,
            max: u64,
        }
        impl Callback for CounterPublisher {
            fn run(&mut self, _ctx: &Context) -> Run {
                if self.next < self.max {
                    let mut out = Output::<u64>::new_default(&mut self.publisher);
                    *out = self.next;
                    out.send();
                    self.next += 1;
                }
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

        /// Required trigger on "trigger", required non-trigger on "gate".
        /// Records every run's observed values; a run missing either value is
        /// a gating violation.
        struct GatedConsumer {
            trigger: Subscriber<u64>,
            gate: Subscriber<u64>,
            runs: Arc<Mutex<Vec<(u64, u64)>>>,
            violations: Arc<AtomicUsize>,
        }
        impl Callback for GatedConsumer {
            fn run(&mut self, _ctx: &Context) -> Run {
                let read_front = |sub: &mut Subscriber<u64>| -> Option<u64> {
                    let guard = sub.read_buffer();
                    guard.front().map(|msg| msg.message)
                };
                match (read_front(&mut self.trigger), read_front(&mut self.gate)) {
                    (Some(a), Some(b)) => self.runs.lock().unwrap().push((a, b)),
                    _ => {
                        self.violations.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Run::new(1)
            }
            fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
                f(&self.trigger);
                f(&self.gate);
            }
            fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
            fn for_each_subscriber_mut<'a>(
                &'a mut self,
                f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
            ) {
                f(&mut self.trigger);
                f(&mut self.gate);
            }
            fn for_each_publisher_mut<'a>(
                &'a mut self,
                _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
            ) {
            }
            fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
                f(PortMut::Subscriber(&mut self.trigger));
                f(PortMut::Subscriber(&mut self.gate));
            }
        }

        let make_periodic = |callback: Box<dyn Callback>, name: &str| {
            let mut node = task::callback::CallbackNode::new_named(callback, name.into());
            node.set_execution_duration_callback(Box::new(|| Duration::ZERO));
            node.set_execution_time_callback(Box::new(|now| Some(now + Duration::from_nanos(1))));
            node
        };

        // The trigger producer publishes a steady stream; the gate producer
        // publishes a single value (100) and then goes quiet — its value must
        // be retained by the consumer's read buffer.
        let trigger_node = make_periodic(
            Box::new(CounterPublisher {
                publisher: Publisher::<u64>::new(PublisherConfig {
                    capacity: 1,
                    channel_name: "trigger".into(),
                }),
                next: 0,
                max: 100,
            }),
            "trigger_producer",
        );
        let gate_node = make_periodic(
            Box::new(CounterPublisher {
                publisher: Publisher::<u64>::new(PublisherConfig {
                    capacity: 1,
                    channel_name: "gate".into(),
                }),
                next: 100,
                max: 101,
            }),
            "gate_producer",
        );

        let runs = Arc::new(Mutex::new(Vec::new()));
        let violations = Arc::new(AtomicUsize::new(0));
        let mut consumer = task::callback::CallbackNode::new_named(
            Box::new(GatedConsumer {
                trigger: Subscriber::<u64>::new(SubscriberConfig {
                    is_optional: false,
                    capacity: 1,
                    is_trigger: true,
                    keep_across_runs: true,
                    channel_name: "trigger".into(),
                }),
                gate: Subscriber::<u64>::new(SubscriberConfig {
                    is_optional: false,
                    capacity: 1,
                    is_trigger: false,
                    keep_across_runs: true,
                    channel_name: "gate".into(),
                }),
                runs: runs.clone(),
                violations: violations.clone(),
            }),
            "consumer".into(),
        );
        consumer.set_execution_duration_callback(Box::new(|| Duration::ZERO));

        let mut nodes = vec![trigger_node, gate_node, consumer];
        task::callback::connect_callback_nodes(&mut nodes).expect("channels connect");

        let mut state = SimulationState::new(1, nodes);
        state.start();
        for _ in 0..20 {
            state.step().unwrap();
        }

        assert_eq!(
            violations.load(Ordering::Relaxed),
            0,
            "consumer must never run while a required input has no value"
        );
        let observed = runs.lock().unwrap();
        assert!(
            observed.len() >= 2,
            "consumer should re-run on new trigger data while the gate value is retained, got {observed:?}"
        );
        for &(_, gate_value) in observed.iter() {
            assert_eq!(
                gate_value, 100,
                "gate value must be the retained single publish"
            );
        }
        // Every run pairs the newest trigger value with the retained gate value.
        assert!(observed.windows(2).all(|w| w[0].0 < w[1].0));
    }
}
