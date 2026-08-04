use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use logging::sorted_log_stream::{
    ReplaySinkMap, SortedLogStreamReader, build_replay_sinks,
};
use task::callback::{Callback, CallbackNode, PortMut, Run};
use task::channel_registry::ChannelRegistry;
use task::context::Context;
use task::executor::ExecutorStopSignal;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::pub_sub::ChannelName;
use task::task_graph_builder::{TaskGraphBuildStep, TaskGraphBuildStepError};
use task::time::FrameworkTime;

const INVALID_NS: i64 = i64::MIN;

pub struct LogSimulationTask {
    reader: SortedLogStreamReader,
    sinks: ReplaySinkMap,
    next_time_ns: Arc<AtomicI64>,
    stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
}

impl Callback for LogSimulationTask {
    fn run(&mut self, ctx: &Context) -> Run {
        let (batch, next_time) = self.reader.read_until(ctx.now);
        for entry in &batch {
            self.sinks.publish(entry);
        }

        match next_time {
            Some(t) => {
                self.next_time_ns
                    .store(t.to_nanoseconds(), Ordering::Relaxed);
            }
            None => {
                self.next_time_ns.store(INVALID_NS, Ordering::Relaxed);
                if let Some(signal) = self.stop_signal.get() {
                    signal.request_stop();
                }
            }
        }

        Run::new(0)
    }

    fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
    fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
        self.sinks.for_each_publisher(f);
    }
    fn for_each_subscriber_mut<'a>(
        &'a mut self,
        _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
    ) {
    }
    fn for_each_publisher_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericPublisher)) {
        self.sinks.for_each_publisher_mut(f);
    }
    fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
        self.sinks.for_each_port_mut(f);
    }
}

pub struct LogSimulationBuildStep {
    reader: Arc<Mutex<Option<SortedLogStreamReader>>>,
    next_time_ns: Arc<AtomicI64>,
    denylist: HashSet<ChannelName>,
    stop_signal_cell: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    first_time: FrameworkTime,
}

impl LogSimulationBuildStep {
    /// Construct a new build step from a log file path.
    ///
    /// Opens the file, creates a streaming reader, and seeds the first
    /// execution time from the earliest logged entry.
    pub fn new(
        path: PathBuf,
        denylist: HashSet<ChannelName>,
        stop_signal_cell: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut log_reader = SortedLogStreamReader::from_path(&path, 65536)?;

        if log_reader.is_empty() {
            return Err("Log file is empty".into());
        }

        let first_time = log_reader
            .peek_time()
            .unwrap_or(FrameworkTime::from_nanoseconds(0));
        let next_time_ns = Arc::new(AtomicI64::new(first_time.to_nanoseconds()));

        Ok(LogSimulationBuildStep {
            reader: Arc::new(Mutex::new(Some(log_reader))),
            next_time_ns,
            denylist,
            stop_signal_cell,
            first_time,
        })
    }

    /// Timestamp of the earliest entry in the log.
    pub fn first_log_time(&self) -> FrameworkTime {
        self.first_time
    }
}

impl TaskGraphBuildStep for LogSimulationBuildStep {
    fn name(&self) -> &str {
        "LogSimulationBuildStep"
    }

    fn build_step(
        &self,
        _nodes: &[CallbackNode],
        channel_registry: &mut ChannelRegistry,
    ) -> Result<Vec<CallbackNode>, TaskGraphBuildStepError> {
        let reader_guard = self.reader.lock().unwrap();
        let reader = reader_guard
            .as_ref()
            .expect("LogSimulationBuildStep: reader already taken; build_step may only be called once");
        let sinks = build_replay_sinks(reader, channel_registry, &self.denylist)?;
        drop(reader_guard);

        if sinks.is_empty() {
            return Ok(vec![]);
        }

        let mut reader_guard = self.reader.lock().unwrap();
        let reader = reader_guard
            .take()
            .expect("LogSimulationBuildStep: reader already taken");
        drop(reader_guard);

        let next_time_ns = self.next_time_ns.clone();
        let stop_signal = self.stop_signal_cell.clone();

        let log_task = LogSimulationTask {
            reader,
            sinks,
            next_time_ns: next_time_ns.clone(),
            stop_signal,
        };

        let mut node = CallbackNode::new_named(Box::new(log_task), "LogSimulationTask".into());
        node.set_execution_duration_callback(Box::new(|| std::time::Duration::ZERO));
        node.set_execution_time_callback(Box::new(move |_now| {
            let ns = next_time_ns.load(Ordering::Relaxed);
            if ns == INVALID_NS {
                None
            } else {
                Some(FrameworkTime::from_nanoseconds(ns))
            }
        }));

        Ok(vec![node])
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};

    use logging::log_file::LogFileWriter;
    use logging::log_file_json::JsonLogFileWriter;
    use task::callback::CallbackViews;
    use task::channel_registry::ChannelRegistry;
    use task::executor::ExecutorStopSignal;
    use task::input::OptionalInput;
    use task::publisher::Publisher;
    use task::subscriber::{Subscriber, SubscriberConfig};
    use task::time::FrameworkTime;

    use crate::state::SimulationState;

    use super::*;

    struct TestStopSignal(Arc<AtomicBool>);
    impl ExecutorStopSignal for TestStopSignal {
        fn request_stop(&self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn serialize_u64(val: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        task::loggable::Loggable::serialize(&val, &mut buf).unwrap();
        buf
    }

    #[test]
    fn test_log_simulation_schedules_by_log_time() {
        let stopped = Arc::new(AtomicBool::new(false));

        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("integer".into());

        // Build a log buffer using JsonLogFileWriter.
        let mut log_buf = Vec::new();
        {
            let mut writer = JsonLogFileWriter::new(&mut log_buf);
            writer
                .store_message(
                    "integer",
                    &task::message::MessageHeader::new(FrameworkTime::from_nanoseconds(1_000)),
                    &serialize_u64(42u64),
                )
                .unwrap();
            writer
                .store_message(
                    "integer",
                    &task::message::MessageHeader::new(FrameworkTime::from_nanoseconds(3_000)),
                    &serialize_u64(7u64),
                )
                .unwrap();
            writer
                .store_message(
                    "integer",
                    &task::message::MessageHeader::new(FrameworkTime::from_nanoseconds(5_000)),
                    &serialize_u64(9u64),
                )
                .unwrap();
        }

        let stop_signal_cell = Arc::new(OnceLock::new());
        let _ = stop_signal_cell
            .set(Arc::new(TestStopSignal(stopped.clone())) as Arc<dyn ExecutorStopSignal>);

        let mut reader =
            SortedLogStreamReader::from_reader(log_buf.as_slice(), 64).unwrap();
        let first_time = reader.peek_time().unwrap();
        let next_time_ns = Arc::new(AtomicI64::new(first_time.to_nanoseconds()));

        let build_step = LogSimulationBuildStep {
            reader: Arc::new(Mutex::new(Some(reader))),
            next_time_ns: next_time_ns.clone(),
            denylist: HashSet::new(),
            stop_signal_cell: stop_signal_cell.clone(),
            first_time,
        };

        let mut channel_registry = ChannelRegistry::new();
        channel_registry.register_channel::<u64>("integer".into());
        let nodes = build_step
            .build_step(&[], &mut channel_registry)
            .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name(), "LogSimulationTask");

        let mut node = nodes.into_iter().next().unwrap();

        let mut sub = Subscriber::<u64>::new(SubscriberConfig {
            is_optional: false,
            capacity: 4,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: "integer".into(),
        });
        {
            let mut pubs = node.callback_mut().collect_publishers_mut();
            let pub_ref: &mut Publisher<u64> = pubs[0]
                .as_any()
                .downcast_mut::<Publisher<u64>>()
                .expect("publisher should be u64");
            pub_ref.add_typed_subscriber(&mut sub);
            pub_ref.allocate_arena();
        }

        let mut state = SimulationState::new(1, vec![node]);
        state.start();

        let mut observed_values: Vec<u64> = Vec::new();

        for _ in 0..20 {
            if stopped.load(Ordering::Relaxed) {
                break;
            }

            let _executed = state.step().unwrap();

            sub.drain_writer_to_reader();
            {
                let mut input = OptionalInput::<u64>::new_downcasted(&mut sub);
                if let Some(val) = input.value() {
                    observed_values.push(*val);
                    input.clear();
                }
            }
        }

        assert_eq!(observed_values, vec![42u64, 7u64, 9u64]);
        assert_eq!(
            state.simulation_time(),
            FrameworkTime::from_nanoseconds(5_000)
        );
        assert!(stopped.load(Ordering::Relaxed));
    }

    #[test]
    fn test_log_simulation_drylists_execution_log() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("integer".into());

        let mut log_buf = Vec::new();
        {
            let mut writer = JsonLogFileWriter::new(&mut log_buf);
            writer
                .store_message(
                    task::execution_log::EXECUTION_LOG_CHANNEL,
                    &task::message::MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
                    &serialize_u64(0u64),
                )
                .unwrap();
            writer
                .store_message(
                    "integer",
                    &task::message::MessageHeader::new(FrameworkTime::from_nanoseconds(500)),
                    &serialize_u64(100u64),
                )
                .unwrap();
        }

        let stop_signal_cell = Arc::new(OnceLock::new());
        let mut reader =
            SortedLogStreamReader::from_reader(log_buf.as_slice(), 64).unwrap();
        let first_time = reader.peek_time().unwrap();
        let next_time_ns = Arc::new(AtomicI64::new(first_time.to_nanoseconds()));

        let build_step = LogSimulationBuildStep {
            reader: Arc::new(Mutex::new(Some(reader))),
            next_time_ns,
            denylist: HashSet::new(),
            stop_signal_cell,
            first_time,
        };

        let mut channel_registry = ChannelRegistry::new();
        channel_registry.register_channel::<u64>("integer".into());
        let nodes = build_step
            .build_step(&[], &mut channel_registry)
            .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].callback().collect_publishers().len(), 1);
        assert_eq!(
            nodes[0].callback().collect_publishers()[0]
                .config()
                .channel_name,
            "integer"
        );
    }

    #[test]
    fn test_log_simulation_denylisted_channels_skipped() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("integer".into());

        let mut log_buf = Vec::new();
        {
            let mut writer = JsonLogFileWriter::new(&mut log_buf);
            writer
                .store_message(
                    "integer",
                    &task::message::MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
                    &[],
                )
                .unwrap();
        }

        let stop_signal_cell = Arc::new(OnceLock::new());
        let mut reader =
            SortedLogStreamReader::from_reader(log_buf.as_slice(), 64).unwrap();
        let first_time = reader.peek_time().unwrap();
        let next_time_ns = Arc::new(AtomicI64::new(first_time.to_nanoseconds()));

        let mut denylist = HashSet::new();
        denylist.insert("integer".to_string());

        let build_step = LogSimulationBuildStep {
            reader: Arc::new(Mutex::new(Some(reader))),
            next_time_ns,
            denylist,
            stop_signal_cell,
            first_time,
        };

        let mut channel_registry = ChannelRegistry::new();
        channel_registry.register_channel::<u64>("integer".into());
        let nodes = build_step
            .build_step(&[], &mut channel_registry)
            .unwrap();
        assert!(nodes.is_empty());
    }
}