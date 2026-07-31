//! Exact replay executor: replays a recorded execution log through the same
//! callback nodes, comparing actual outputs against expected outputs.
//!
//! # Architecture
//!
//! - [`log_reader`] parses the log file (both ordinary log and execution-log
//!   records), extracts the descriptor and a time-ordered list of executions.
//! - [`scheduler`] drives the replay loop, yielding one execution at a time.
//! - [`replay_task`] manages persistent hydration publishers, capture
//!   subscribers, and the per-execution hydrate/run/compare logic.
//! - [`error`] defines structured error types.
//! - [`ExactReplayExecutor`] ties everything together with the standard
//!   [`task::executor::Executor`] lifecycle.

pub mod error;
pub(crate) mod log_reader;
pub(crate) mod replay_task;
pub(crate) mod scheduler;

pub use crate::error::{ExactReplayExecutorError, ReplayError};
pub use crate::replay_task::DivergencePolicy;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use logging::log_file::LogFileReader;
use task::callback::CallbackNode;
use task::channel_registry::ChannelRegistry;
use task::executor::{Executor, ExecutorStopSignal};
use task::time::FrameworkTime;

use crate::log_reader::parse_replay_log;
use crate::replay_task::{ReplayNodeState, replay_execution};
use crate::scheduler::ReplayScheduler;

pub(crate) const DEFAULT_DIVERGENCE_POLICY: DivergencePolicy = DivergencePolicy::Strict;

pub struct StopSignal(Arc<AtomicBool>);

impl ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Configuration for the exact replay executor.
pub struct ExactReplayConfig {
    /// The callback nodes to replay. These should be fresh copies of the
    /// original nodes (not the ones already wired into a live executor).
    pub nodes: Vec<CallbackNode>,
    /// Channel registry containing serializers, deserializers, and publisher
    /// factories for every channel referenced in the log. Output serialization
    /// is performed by the logging crate using the registry's serializers.
    pub registry: ChannelRegistry,
    /// Log file reader populated with the recorded log data.
    pub log_reader: Box<dyn LogFileReader>,
    /// Divergence policy: `Strict` stops on first mismatch, `BestEffort`
    /// continues collecting errors.
    pub divergence_policy: DivergencePolicy,
}

pub struct ExactReplayExecutor {
    nodes: Vec<CallbackNode>,
    registry: ChannelRegistry,
    scheduler: Option<ReplayScheduler>,
    // Threads where execution is run off of
    execution_threads: Vec<thread::JoinHandle<()>>,
    // Other threads may swap this on/off to stop
    should_run: Arc<AtomicBool>,
    divergence_policy: DivergencePolicy,
    /// Errors collected during replay (shared between the replay thread and
    /// the main thread on stop).
    collected_errors: Arc<std::sync::Mutex<Vec<ReplayError>>>,
}

impl ExactReplayExecutor {
    pub fn new(config: ExactReplayConfig) -> Result<Self, ReplayError> {
        // Parse the log file at construction time so errors surface early.
        let replay_log = parse_replay_log(config.log_reader.as_ref())?;

        // Validate the descriptor against the supplied nodes.
        validate_descriptor(&replay_log.descriptor, &config.nodes, &config.registry)?;

        // Validate descriptor-less executions (infra nodes allowed).
        validate_descriptor_less_executions(&replay_log.descriptor_less_executions, &config.nodes)?;

        let scheduler = ReplayScheduler::new(replay_log.executions);

        Ok(ExactReplayExecutor {
            nodes: config.nodes,
            registry: config.registry,
            scheduler: Some(scheduler),
            execution_threads: Vec::new(),
            should_run: Arc::new(AtomicBool::new(false)),
            divergence_policy: config.divergence_policy,
            collected_errors: Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }

    /// Return the number of replay executions parsed from the log.
    pub fn execution_count(&self) -> usize {
        self.scheduler.as_ref().map(|s| s.len()).unwrap_or(0)
    }

    /// Return the number of executions consumed so far.
    pub fn consumed_count(&self) -> usize {
        self.scheduler.as_ref().map(|s| s.consumed()).unwrap_or(0)
    }

    /// Collect all replay errors accumulated so far.
    pub fn replay_errors(&self) -> Vec<ReplayError> {
        self.collected_errors
            .lock()
            .expect("error lock poisoned")
            .clone()
    }
}

/// Validate the parsed `ExecutionLogDescriptor` against the rebuilt callback
/// nodes: check that every node/port/channel referenced in the descriptor
/// exists in the nodes, that channel names match, and that channels appearing
/// in replay records have registered deserializers/publisher factories in the
/// registry.
fn validate_descriptor(
    descriptor: &task::execution_log::ExecutionLogDescriptor,
    nodes: &[CallbackNode],
    registry: &ChannelRegistry,
) -> Result<(), ReplayError> {
    use task::callback::CallbackViews;

    for (&node_idx, cd) in &descriptor.index_to_callbacks {
        // Check node index is valid.
        if node_idx >= nodes.len() {
            return Err(ReplayError::InvalidCallbackNodeIndex {
                index: node_idx,
                node_count: nodes.len(),
            });
        }
        let node = &nodes[node_idx];
        let node_name = node.name().to_owned();

        // Check subscriber ordinals and channel registrations.
        let subs = node.callback().collect_subscribers();
        for (&ordinal, desc_ch) in &cd.subscriber_index_to_channel_name {
            if ordinal >= subs.len() {
                return Err(ReplayError::InvalidSubscriberOrdinal {
                    node: node_name.clone(),
                    ordinal: ordinal as u16,
                    subscriber_count: subs.len(),
                });
            }
            let actual_ch = &subs[ordinal].config().channel_name;
            if desc_ch != actual_ch {
                return Err(ReplayError::OutputMismatch {
                    node: node_name.clone(),
                    channel: desc_ch.clone(),
                    details: format!(
                        "descriptor subscriber ordinal {ordinal} channel '{desc_ch}' \
                         does not match node channel '{actual_ch}'"
                    ),
                });
            }
            // Verify the channel has a registered deserializer (needed for
            // hydration).  Allow channels that have no logged messages for
            // this node — they simply won't be hydrated.
            if let Some(type_id) = registry.channel_type(desc_ch) {
                if registry.deserializer_for(type_id).is_none() {
                    return Err(ReplayError::UnregisteredDeserializer {
                        channel: desc_ch.clone(),
                        node: node_name.clone(),
                    });
                }
                if registry.channel_publisher_factory(type_id).is_none() {
                    return Err(ReplayError::UnregisteredDeserializer {
                        channel: desc_ch.clone(),
                        node: node_name.clone(),
                    });
                }
            }
        }

        // Check publisher ordinals and channel registrations.
        let pubs = node.callback().collect_publishers();
        for (&ordinal, desc_ch) in &cd.publisher_index_to_channel_name {
            if ordinal >= pubs.len() {
                return Err(ReplayError::InvalidPublisherOrdinal {
                    node: node_name.clone(),
                    ordinal: ordinal as u16,
                    publisher_count: pubs.len(),
                });
            }
            let actual_ch = &pubs[ordinal].config().channel_name;
            if desc_ch != actual_ch {
                return Err(ReplayError::OutputMismatch {
                    node: node_name.clone(),
                    channel: desc_ch.clone(),
                    details: format!(
                        "descriptor publisher ordinal {ordinal} channel '{desc_ch}' \
                         does not match node channel '{actual_ch}'"
                    ),
                });
            }
            // Verify the publisher channel has a registered serializer
            // (needed for output capture).
            if let Some(type_id) = registry.channel_type(desc_ch) {
                if registry.serializer_for(type_id).is_none() {
                    return Err(ReplayError::UnregisteredOutputCapture {
                        channel: desc_ch.clone(),
                        node: node_name.clone(),
                    });
                }
            }
        }
    }

    Ok(())
}

/// Validate descriptor-less execution records.  An execution for a node
/// index that is out of range is always an error.  In-range indices must
/// be infrastructure nodes (LogTask); otherwise fail.
fn validate_descriptor_less_executions(
    descriptor_less: &[(usize, FrameworkTime)],
    nodes: &[CallbackNode],
) -> Result<(), ReplayError> {
    for (node_idx, _time) in descriptor_less {
        if *node_idx >= nodes.len() {
            return Err(ReplayError::InvalidCallbackNodeIndex {
                index: *node_idx,
                node_count: nodes.len(),
            });
        }
        let node_name = nodes[*node_idx].name().to_owned();
        if !node_name.starts_with("LogTask") {
            return Err(ReplayError::DescriptorlessApplicationNode {
                index: *node_idx,
                node_name,
            });
        }
    }
    Ok(())
}

impl Executor for ExactReplayExecutor {
    type Error = ExactReplayExecutorError;

    fn start(&mut self) {
        self.should_run.store(true, Ordering::Release);
        let should_run = self.should_run.clone();
        let mut scheduler = self.scheduler.take().expect("executor already started");
        let mut nodes = std::mem::take(&mut self.nodes);
        let registry = self.registry.clone();
        let divergence_policy = self.divergence_policy;
        let collected_errors = self.collected_errors.clone();

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
                    divergence_policy,
                    &mut errors,
                );

                if !errors.is_empty() {
                    let mut errs = collected_errors.lock().expect("error lock poisoned");
                    errs.extend(errors);
                    if divergence_policy == DivergencePolicy::Strict {
                        should_run.store(false, Ordering::Release);
                        break;
                    }
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

        let replay_errors: Vec<ReplayError> = self
            .collected_errors
            .lock()
            .expect("error lock poisoned")
            .drain(..)
            .collect();

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

    use task::callback::{Callback, PortMut, Run};
    use task::context::Context;
    use task::execution_log::{
        Direction, EXECUTION_LOG_CHANNEL, EXECUTION_LOG_DESCRIPTOR_CHANNEL, ExecutionLogDescriptor,
        ExecutionLogEntry, LoggedMessage,
    };
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
            .store_message(
                EXECUTION_LOG_DESCRIPTOR_CHANNEL,
                &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
                &desc_bytes,
            )
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

        drop(writer);
    }

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
        let config = ExactReplayConfig {
            nodes: vec![node],
            registry,
            log_reader: Box::new(reader),
            divergence_policy: DivergencePolicy::Strict,
        };

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
            .store_message(
                EXECUTION_LOG_DESCRIPTOR_CHANNEL,
                &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
                &desc_bytes,
            )
            .unwrap();
        drop(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("Empty");
        let config = ExactReplayConfig {
            nodes: vec![node],
            registry,
            log_reader: Box::new(reader),
            divergence_policy: DivergencePolicy::Strict,
        };

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
            .store_message(
                EXECUTION_LOG_DESCRIPTOR_CHANNEL,
                &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
                &desc_bytes,
            )
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
        drop(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("Dropped");
        let config = ExactReplayConfig {
            nodes: vec![node],
            registry,
            log_reader: Box::new(reader),
            divergence_policy: DivergencePolicy::Strict,
        };

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
        drop(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let node = make_passthrough_node("NoDesc");
        let config = ExactReplayConfig {
            nodes: vec![node],
            registry,
            log_reader: Box::new(reader),
            divergence_policy: DivergencePolicy::Strict,
        };

        let result = ExactReplayExecutor::new(config);
        let Err(err) = result else {
            panic!("expected error, got Ok");
        };
        assert!(matches!(err, ReplayError::MissingOrInvalidDescriptor(_)));
    }
}
