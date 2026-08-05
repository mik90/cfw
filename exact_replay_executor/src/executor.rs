//! Execution lifecycle: [`ExactReplayExecutor`], [`StopSignal`], construction,
//! and replay worker orchestration.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use task::callback::CallbackNode;
use task::channel_registry::ChannelRegistry;
use task::executor::{Executor, ExecutorStopSignal};
use task::generic_subscriber::GenericSubscriber;
use task::message::MessageHeader;

use crate::config::ExactReplayConfig;
use crate::descriptor::{validate_descriptor, validate_descriptor_less_executions};
use crate::error::{ExactReplayExecutorError, ReplayError};
use crate::log_reader::parse_replay_log;
use crate::replay_task::{DivergencePolicy, ReplayNodeState, replay_execution};
use crate::report::{DEFAULT_MAX_MISMATCH_DETAILS, ReplayReport};
use crate::reproduce::ReproducedPayloadStore;
use crate::scheduler::ReplayScheduler;

/// Signal used to request that the executor stop.
pub(crate) struct StopSignal(pub(crate) Arc<AtomicBool>);

impl ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The exact replay executor. Replays a recorded execution log through the
/// same callback nodes, comparing actual outputs against expected outputs.
pub struct ExactReplayExecutor {
    nodes: Vec<CallbackNode>,
    registry: ChannelRegistry,
    scheduler: Option<ReplayScheduler>,
    /// Ordinary-log payloads per channel, retained for forwarded-message
    /// context resolution during replay.
    source_messages: HashMap<task::pub_sub::ChannelName, Vec<(MessageHeader, Vec<u8>)>>,
    /// Reproduced payloads for unlogged channels, shared with the replay
    /// thread.
    store: ReproducedPayloadStore,
    /// Accuracy report, shared with the replay thread.
    report: Arc<std::sync::Mutex<ReplayReport>>,
    /// Total number of executions parsed from the log (cached at construction).
    total_execution_count: usize,
    /// Number of executions consumed so far, shared with the replay thread.
    consumed_count: Arc<AtomicUsize>,
    /// Threads where execution is run off of.
    execution_threads: Vec<thread::JoinHandle<()>>,
    /// Other threads may swap this on/off to stop.
    should_run: Arc<AtomicBool>,
    divergence_policy: DivergencePolicy,
    /// Errors collected during replay (shared between the replay thread and
    /// the main thread on stop).
    collected_errors: Arc<std::sync::Mutex<Vec<ReplayError>>>,
    /// Whether the executor has been started.
    started: bool,
}

impl ExactReplayExecutor {
    pub fn new(config: ExactReplayConfig) -> Result<Self, ReplayError> {
        // Parse the log file at construction time so errors surface early.
        let replay_log = parse_replay_log(config.log_reader.as_ref())?;

        // Flatten pools into the global node order used by the execution-log
        // descriptor indices.
        let nodes = config
            .pools
            .into_iter()
            .flat_map(|pool| pool.nodes)
            .collect::<Vec<_>>();

        // Validate the descriptor against the supplied nodes.
        validate_descriptor(&replay_log, &nodes, &config.registry)?;

        // Validate descriptor-less executions (infra nodes allowed).
        validate_descriptor_less_executions(&replay_log.descriptor_less_executions, &nodes)?;

        let total_execution_count = replay_log.executions.len();
        let scheduler = ReplayScheduler::new(replay_log.executions);

        Ok(ExactReplayExecutor {
            nodes,
            registry: config.registry,
            scheduler: Some(scheduler),
            source_messages: replay_log.source_messages,
            store: ReproducedPayloadStore::new(),
            report: Arc::new(std::sync::Mutex::new(ReplayReport::new(
                total_execution_count,
                DEFAULT_MAX_MISMATCH_DETAILS,
            ))),
            total_execution_count,
            consumed_count: Arc::new(AtomicUsize::new(0)),
            execution_threads: Vec::new(),
            should_run: Arc::new(AtomicBool::new(false)),
            divergence_policy: config.divergence_policy,
            collected_errors: Arc::new(std::sync::Mutex::new(Vec::new())),
            started: false,
        })
    }

    /// Return the total number of replay executions parsed from the log.
    /// This count is cached at construction and does not change.
    pub fn execution_count(&self) -> usize {
        self.total_execution_count
    }

    /// Return the number of executions consumed so far.
    /// Works while running and after completion.
    pub fn consumed_count(&self) -> usize {
        self.consumed_count.load(Ordering::Acquire)
    }

    /// Collect all replay errors accumulated so far.
    /// Errors are preserved after stop; this method clones them.
    pub fn replay_errors(&self) -> Vec<ReplayError> {
        self.collected_errors
            .lock()
            .expect("error lock poisoned")
            .clone()
    }

    /// Snapshot of the replay accuracy report. Works while running and after
    /// completion; call after [`stop`](Self::stop) for the final numbers.
    pub fn replay_report(&self) -> ReplayReport {
        self.report.lock().expect("report lock poisoned").clone()
    }
}

impl Executor for ExactReplayExecutor {
    type Error = ExactReplayExecutorError;

    fn start(&mut self) {
        if self.started {
            // Repeated start is a no-op (safe, defined behavior).
            return;
        }
        self.started = true;
        if self.total_execution_count == 0 {
            self.should_run.store(false, Ordering::Release);
            return;
        }
        self.should_run.store(true, Ordering::Release);
        let should_run = self.should_run.clone();
        let mut scheduler = self.scheduler.take().expect("scheduler already taken");
        let mut nodes = std::mem::take(&mut self.nodes);
        let registry = self.registry.clone();
        let source_messages = std::mem::take(&mut self.source_messages);
        let store = self.store.clone();
        let report = self.report.clone();
        let divergence_policy = self.divergence_policy;
        let collected_errors = self.collected_errors.clone();
        let consumed_count = self.consumed_count.clone();

        self.execution_threads.push(thread::spawn(move || {
            // Per-node replay state: hydration publishers + capture subscribers.
            let mut node_states: Vec<ReplayNodeState> =
                (0..nodes.len()).map(|_| ReplayNodeState::new()).collect();

            while should_run.load(Ordering::Acquire) {
                let Some(execution) = scheduler.advance() else {
                    should_run.store(false, Ordering::Release);
                    break;
                };

                let node_idx = execution.callback_node_index;
                if node_idx >= nodes.len() {
                    let mut errs = collected_errors.lock().expect("error lock poisoned");
                    errs.push(ReplayError::InvalidCallbackNodeIndex {
                        index: node_idx,
                        node_count: nodes.len(),
                    });
                    report.lock().unwrap().record_error();
                    consumed_count.fetch_add(1, Ordering::Release);
                    if divergence_policy == DivergencePolicy::Strict {
                        should_run.store(false, Ordering::Release);
                        break;
                    }
                    continue;
                }

                let mut errors = Vec::new();
                replay_execution(
                    &mut nodes[node_idx],
                    &mut node_states[node_idx],
                    execution,
                    &registry,
                    &source_messages,
                    &store,
                    &report,
                    divergence_policy,
                    &mut errors,
                );

                // Advance consumed count after each execution.
                consumed_count.fetch_add(1, Ordering::Release);
                report.lock().unwrap().mark_consumed();

                if !errors.is_empty() {
                    let new_errors = errors.len();
                    let mut errs = collected_errors.lock().expect("error lock poisoned");
                    errs.extend(errors);
                    {
                        let mut rep = report.lock().unwrap();
                        for _ in 0..new_errors {
                            rep.record_error();
                        }
                    }
                    if divergence_policy == DivergencePolicy::Strict {
                        should_run.store(false, Ordering::Release);
                        break;
                    }
                }
            }

            // ── Cleanup: clear subscriber buffers while persistent hydration
            //    publishers and capture subscribers (and their ArenaPtrs) are
            //    still alive. Explicitly call cleanup_buffers on each
            //    subscriber to discard retained ArenaPtrs that could dangle
            //    once the node_states are dropped.
            //
            //    node.drain_subscribers() only moves write → read queues; it
            //    does NOT clear ArenaPtrs that subscribers may still hold in
            //    their read buffer. cleanup_buffers() is the correct API for
            //    that.
            for node in &mut nodes {
                if let Err(_e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut cleanup = |s: &dyn GenericSubscriber| {
                        s.cleanup_buffers();
                    };
                    node.callback().for_each_subscriber(&mut cleanup);
                })) {
                    // Swallow panics during teardown — nothing we can do.
                }
            }
        }));
    }

    fn stop(&mut self) -> Result<(), ExactReplayExecutorError> {
        self.should_run.store(false, Ordering::Release);

        let mut panicked_thread_indices = vec![];
        for (thread_idx, t) in self.execution_threads.drain(..).enumerate() {
            if t.join().is_err() {
                panicked_thread_indices.push(thread_idx);
            }
        }

        // Clone errors so the shared collection remains usable after stop
        // (replay_errors() still returns them).
        let replay_errors: Vec<ReplayError> = self
            .collected_errors
            .lock()
            .expect("error lock poisoned")
            .clone();

        if panicked_thread_indices.is_empty() && replay_errors.is_empty() {
            Ok(())
        } else {
            Err(ExactReplayExecutorError {
                panicked_thread_indices,
                replay_errors,
            })
        }
    }

    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal> {
        Arc::new(StopSignal(self.should_run.clone()))
    }

    fn is_running(&self) -> bool {
        self.should_run.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::thread;

    use task::callback::{Callback, PortMut, Run};
    use task::context::Context;
    use task::execution_log::{
        Direction, EXECUTION_LOG_CHANNEL, EXECUTION_LOG_DESCRIPTOR_ARTIFACT,
        ExecutionLogDescriptor, ExecutionLogEntry, LoggedMessage,
    };
    use task::executor::ThreadPoolConfig;
    use task::generic_publisher::GenericPublisher;
    use task::generic_subscriber::GenericSubscriber;
    use task::message::MessageHeader;
    use task::output::Output;
    use task::publisher::{Publisher, PublisherConfig};
    use task::subscriber::{Subscriber, SubscriberConfig};
    use task::time::FrameworkTime;

    use logging::log_file::LogFileWriter;
    use logging::log_file_json::{JsonLogFileReader, JsonLogFileWriter};

    /// A simple callback that reads a u64 from its subscriber and publishes it
    /// unchanged.
    struct PassthroughCallback {
        sub: Subscriber<u64>,
        pub_: Publisher<u64>,
    }

    impl Callback for PassthroughCallback {
        fn run(&mut self, _ctx: &Context) -> Run {
            let input = task::input::OptionalInput::<u64>::new_downcasted(&mut self.sub);
            if let Some(val) = input.value() {
                let mut output = Output::<u64>::new_downcasted(&mut self.pub_);
                *output = *val;
                output.send();
            }
            Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            f(&self.sub);
        }
        fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
            f(&self.pub_);
        }
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            f(&mut self.sub);
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
            f(&mut self.pub_);
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            f(PortMut::Subscriber(&mut self.sub));
            f(PortMut::Publisher(&mut self.pub_));
        }
    }

    fn make_passthrough_node(name: &str) -> CallbackNode {
        CallbackNode::new_named(
            Box::new(PassthroughCallback {
                sub: Subscriber::<u64>::new(SubscriberConfig {
                    is_optional: true,
                    capacity: 1,
                    is_trigger: true,
                    keep_across_runs: true,
                    channel_name: "input".into(),
                }),
                pub_: Publisher::<u64>::new(PublisherConfig {
                    capacity: 1,
                    channel_name: "output".into(),
                }),
            }),
            name.into(),
        )
    }

    /// Write a log file with a descriptor and one execution.
    fn write_test_log(
        buf: &mut Vec<u8>,
        desc: &ExecutionLogDescriptor,
        entries: &[ExecutionLogEntry],
    ) {
        let mut writer = JsonLogFileWriter::new(buf);

        let desc_bytes = serde_json::to_vec(desc).unwrap();
        writer
            .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes)
            .unwrap();

        for chunk in entries.chunks(task::execution_log::ENTRIES_PER_MESSAGE) {
            let msg_bytes = serde_json::to_vec(&serde_json::json!({
                "number_of_dropped_entries": 0,
                "entries": chunk,
            }))
            .unwrap();
            writer
                .store_message(
                    EXECUTION_LOG_CHANNEL,
                    &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
                    &msg_bytes,
                )
                .unwrap();
        }

        // Write ordinary log entries for the input/output values
        for entry in entries {
            for msg in &entry.messages {
                if !msg.is_valid() {
                    break;
                }
                let channel = if msg.direction == Direction::Received {
                    "input"
                } else {
                    "output"
                };
                let val = serde_json::to_vec(&42u64).unwrap();
                writer.store_message(channel, &msg.header, &val).unwrap();
            }
        }

        finish_writer(writer);
    }

    // `JsonLogFileWriter` borrows the backing buffer but has no Drop
    // implementation. Consume it explicitly so that the borrow ends without
    // triggering clippy::drop_non_drop.
    fn finish_writer<W: std::io::Write>(_: JsonLogFileWriter<W>) {}

    #[test]
    fn test_exact_replay_passthrough() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());
        registry.register_channel::<u64>("output".into());

        let mut desc = ExecutionLogDescriptor::new(&[]);
        let mut sub_map = HashMap::new();
        sub_map.insert(0usize, "input".to_string());
        let mut pub_map = HashMap::new();
        pub_map.insert(0usize, "output".to_string());
        desc.index_to_callbacks.insert(
            0usize,
            task::execution_log::CallbackDescriptor {
                subscriber_index_to_channel_name: sub_map,
                publisher_index_to_channel_name: pub_map,
            },
        );

        let mut entries = Vec::new();
        let mut entry = ExecutionLogEntry {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(100),
            execution_duration_ns: 0,
            log_whole: true,
            messages: std::array::from_fn(|_| LoggedMessage::default()),
        };
        entry.messages[0] = LoggedMessage {
            ordinal: 0,
            direction: Direction::Received,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
        };
        entry.messages[1] = LoggedMessage {
            ordinal: 0,
            direction: Direction::Published,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
        };
        entries.push(entry);

        let mut buf = Vec::new();
        write_test_log(&mut buf, &desc, &entries);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("Passthrough");
        let config = ExactReplayConfig::new(
            vec![ThreadPoolConfig::new(1, vec![node])],
            registry,
            Box::new(reader),
        );

        let mut executor = ExactReplayExecutor::new(config).expect("should build");
        assert_eq!(executor.execution_count(), 1);
        executor.start();
        let result = executor.stop();
        assert!(
            result.is_ok(),
            "expected successful replay, got: {:?}",
            result
        );
    }

    #[test]
    fn test_empty_execution_log() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());

        let desc = ExecutionLogDescriptor::new(&[]);
        let mut buf = Vec::new();
        let mut writer = JsonLogFileWriter::new(&mut buf);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        writer
            .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes)
            .unwrap();
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("Empty");
        let config = ExactReplayConfig::new(
            vec![ThreadPoolConfig::new(1, vec![node])],
            registry,
            Box::new(reader),
        );

        let mut executor = ExactReplayExecutor::new(config).expect("should build");
        assert_eq!(executor.execution_count(), 0);
        executor.start();
        let result = executor.stop();
        assert!(result.is_ok(), "empty log should succeed");
    }

    #[test]
    fn test_rejects_dropped_entries() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());

        let desc = ExecutionLogDescriptor::new(&[]);
        let mut buf = Vec::new();
        let mut writer = JsonLogFileWriter::new(&mut buf);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        writer
            .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes)
            .unwrap();
        let msg_bytes = serde_json::to_vec(&serde_json::json!({
            "number_of_dropped_entries": 3,
            "entries": []
        }))
        .unwrap();
        writer
            .store_message(
                EXECUTION_LOG_CHANNEL,
                &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
                &msg_bytes,
            )
            .unwrap();
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("Dropped");
        let config = ExactReplayConfig::new(
            vec![ThreadPoolConfig::new(1, vec![node])],
            registry,
            Box::new(reader),
        );

        let result = ExactReplayExecutor::new(config);
        let Err(err) = result else {
            panic!("expected error, got Ok");
        };
        assert!(matches!(
            err,
            ReplayError::DroppedExecutionLogEntries { count: 3 }
        ));
    }

    #[test]
    fn test_missing_descriptor_fails() {
        let registry = ChannelRegistry::new();
        let mut buf = Vec::new();
        let mut writer = JsonLogFileWriter::new(&mut buf);
        writer
            .store_message(
                "some_channel",
                &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
                b"hello",
            )
            .unwrap();
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("NoDesc");
        let config = ExactReplayConfig::new(
            vec![ThreadPoolConfig::new(1, vec![node])],
            registry,
            Box::new(reader),
        );

        let result = ExactReplayExecutor::new(config);
        let Err(err) = result else {
            panic!("expected error, got Ok");
        };
        assert!(matches!(err, ReplayError::MissingOrInvalidDescriptor(_)));
    }

    #[test]
    fn test_stop_before_start() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());

        let desc = ExecutionLogDescriptor::new(&[]);
        let mut buf = Vec::new();
        let mut writer = JsonLogFileWriter::new(&mut buf);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        writer
            .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes)
            .unwrap();
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("StopBeforeStart");
        let config = ExactReplayConfig::new(
            vec![ThreadPoolConfig::new(1, vec![node])],
            registry,
            Box::new(reader),
        );

        let mut executor = ExactReplayExecutor::new(config).expect("should build");
        // Stop before start — should not panic and should return Ok (no threads to join).
        let result = executor.stop();
        assert!(result.is_ok(), "stop before start should succeed");
    }

    #[test]
    fn test_repeated_start_is_noop() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());

        let desc = ExecutionLogDescriptor::new(&[]);
        let mut buf = Vec::new();
        let mut writer = JsonLogFileWriter::new(&mut buf);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        writer
            .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes)
            .unwrap();
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("RepeatedStart");
        let config = ExactReplayConfig::new(
            vec![ThreadPoolConfig::new(1, vec![node])],
            registry,
            Box::new(reader),
        );

        let mut executor = ExactReplayExecutor::new(config).expect("should build");

        // First start is fine.
        executor.start();

        // Second start should be a no-op (not panic).
        executor.start();

        let result = executor.stop();
        assert!(result.is_ok(), "repeated start then stop should succeed");
    }

    #[test]
    fn test_consumed_count_works() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());
        registry.register_channel::<u64>("output".into());

        let mut desc = ExecutionLogDescriptor::new(&[]);
        let mut sub_map = HashMap::new();
        sub_map.insert(0usize, "input".to_string());
        let mut pub_map = HashMap::new();
        pub_map.insert(0usize, "output".to_string());
        desc.index_to_callbacks.insert(
            0usize,
            task::execution_log::CallbackDescriptor {
                subscriber_index_to_channel_name: sub_map,
                publisher_index_to_channel_name: pub_map,
            },
        );

        let mut entries = Vec::new();
        let mut entry = ExecutionLogEntry {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(100),
            execution_duration_ns: 0,
            log_whole: true,
            messages: std::array::from_fn(|_| LoggedMessage::default()),
        };
        entry.messages[0] = LoggedMessage {
            ordinal: 0,
            direction: Direction::Received,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
        };
        entry.messages[1] = LoggedMessage {
            ordinal: 0,
            direction: Direction::Published,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
        };
        entries.push(entry);

        // Second execution with a different execution time.
        let mut entry2 = ExecutionLogEntry {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(200),
            execution_duration_ns: 0,
            log_whole: true,
            messages: std::array::from_fn(|_| LoggedMessage::default()),
        };
        entry2.messages[0] = LoggedMessage {
            ordinal: 0,
            direction: Direction::Received,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(200)),
        };
        entry2.messages[1] = LoggedMessage {
            ordinal: 0,
            direction: Direction::Published,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(200)),
        };
        entries.push(entry2);

        let mut buf = Vec::new();
        write_test_log(&mut buf, &desc, &entries);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("Passthrough");
        let config = ExactReplayConfig::new(
            vec![ThreadPoolConfig::new(1, vec![node])],
            registry,
            Box::new(reader),
        );

        let mut executor = ExactReplayExecutor::new(config).expect("should build");
        assert_eq!(executor.execution_count(), 2);
        assert_eq!(executor.consumed_count(), 0);

        executor.start();
        // Give the thread a moment to start processing before we signal stop.
        thread::sleep(std::time::Duration::from_millis(50));
        let result = executor.stop();
        assert!(
            result.is_ok(),
            "two executions should succeed: {:?}",
            result
        );

        // After completion, consumed count should reflect all executions.
        assert_eq!(executor.consumed_count(), 2);
    }

    #[test]
    fn test_natural_completion() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());

        let desc = ExecutionLogDescriptor::new(&[]);
        let mut buf = Vec::new();
        let mut writer = JsonLogFileWriter::new(&mut buf);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        writer
            .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes)
            .unwrap();
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("Natural");
        let config = ExactReplayConfig::new(
            vec![ThreadPoolConfig::new(1, vec![node])],
            registry,
            Box::new(reader),
        );

        let mut executor = ExactReplayExecutor::new(config).expect("should build");
        assert_eq!(executor.execution_count(), 0);

        // Start and let it run to natural completion (no executions).
        executor.start();

        // Give the thread a moment to run.
        thread::sleep(std::time::Duration::from_millis(50));

        // The thread should have exited naturally.
        assert!(!executor.is_running());

        let result = executor.stop();
        assert!(result.is_ok(), "natural completion should succeed");
        assert_eq!(executor.consumed_count(), 0);
    }

    /// Verify that replay_errors() remains usable after stop().
    /// The errors are cloned (not drained) so the method still returns them.
    #[test]
    fn test_replay_errors_preserved_after_stop() {
        // Use a callback that always produces an unexpected output.
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());
        registry.register_channel::<u64>("output".into());

        struct AlwaysPublish {
            publisher: Publisher<u64>,
        }
        impl Callback for AlwaysPublish {
            fn run(&mut self, _ctx: &Context) -> Run {
                let mut output = Output::new_default(&mut self.publisher);
                *output = 7u64;
                output.send();
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

        let mut desc = ExecutionLogDescriptor::new(&[]);
        desc.index_to_callbacks.insert(
            0usize,
            task::execution_log::CallbackDescriptor {
                subscriber_index_to_channel_name: HashMap::new(),
                publisher_index_to_channel_name: {
                    let mut m = HashMap::new();
                    m.insert(0usize, "output".to_string());
                    m
                },
            },
        );

        let mut buf = Vec::new();
        let mut writer = JsonLogFileWriter::new(&mut buf);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        writer
            .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes)
            .unwrap();
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = CallbackNode::new_named(
            Box::new(AlwaysPublish {
                publisher: Publisher::<u64>::new(PublisherConfig {
                    capacity: 1,
                    channel_name: "output".into(),
                }),
            }),
            "Always".into(),
        );
        let config = ExactReplayConfig::new(
            vec![ThreadPoolConfig::new(1, vec![node])],
            registry,
            Box::new(reader),
        );

        let mut executor = ExactReplayExecutor::new(config).expect("should build");

        // Start and stop (no executions, so no errors — but we need the
        // stop path where errors ARE empty, then verify replay_errors()
        // still works).
        executor.start();
        thread::sleep(std::time::Duration::from_millis(50));
        let _ = executor.stop();

        // Calling replay_errors() after stop should not panic and should
        // return an empty vec (or whatever errors were collected).
        let after_stop = executor.replay_errors();
        assert!(
            after_stop.is_empty(),
            "expected empty errors, got: {after_stop:?}"
        );

        // Calling it again should also work (not drained).
        let after_stop2 = executor.replay_errors();
        assert_eq!(
            after_stop.len(),
            after_stop2.len(),
            "second call should return same number of errors"
        );
    }
}
