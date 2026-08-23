use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use logging::sorted_log_stream::{ReplaySinkMap, SortedLogStreamReader, build_replay_sinks};
use task::callback::{Callback, CallbackNode, PortMut, Run};
use task::channel_registry::ChannelRegistry;
use task::context::Context;
use task::executor::ExecutorStopSignal;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::pub_sub::ChannelName;
use task::task_graph_builder::{TaskGraphBuildStep, TaskGraphBuildStepError};
use task::time::FrameworkTime;

use crate::LiveReplayConfig;

pub struct ReplayTask {
    reader: SortedLogStreamReader,
    sinks: ReplaySinkMap,
    stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
}

impl Callback for ReplayTask {
    fn run(&mut self, ctx: &Context) -> Run {
        let (batch, next_time) = self.reader.read_until(ctx.now);
        for entry in &batch {
            self.sinks.publish(entry);
        }

        let done = next_time.is_none();
        if done {
            if let Some(signal) = self.stop_signal.get() {
                signal.request_stop();
            }
            Run::new(0)
        } else {
            Run::new(1)
        }
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

pub struct ReplayBuildStep {
    reader: Arc<Mutex<Option<SortedLogStreamReader>>>,
    first_time: FrameworkTime,
    denylist: HashSet<ChannelName>,
    stop_signal_cell: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
}

impl TaskGraphBuildStep for ReplayBuildStep {
    fn name(&self) -> &str {
        "ReplayBuildStep"
    }

    fn build_step(
        &self,
        _nodes: &[CallbackNode],
        channel_registry: &mut ChannelRegistry,
    ) -> Result<Vec<CallbackNode>, TaskGraphBuildStepError> {
        let reader_guard = self.reader.lock().unwrap();
        let reader = reader_guard
            .as_ref()
            .expect("ReplayBuildStep: reader already taken; build_step may only be called once");
        let sinks = build_replay_sinks(reader, channel_registry, &self.denylist)?;
        drop(reader_guard);

        if sinks.is_empty() {
            return Ok(vec![]);
        }

        let mut reader_guard = self.reader.lock().unwrap();
        let reader = reader_guard
            .take()
            .expect("ReplayBuildStep: reader already taken");
        drop(reader_guard);

        let first_time = self.first_time;
        let stop_signal = self.stop_signal_cell.clone();

        let replay_task = ReplayTask {
            reader,
            sinks,
            stop_signal,
        };

        let mut node = CallbackNode::new_named(Box::new(replay_task), "ReplayTask".into());
        node.set_execution_duration_callback(Box::new(|| std::time::Duration::ZERO));
        node.set_execution_time_callback(Box::new(move |_now| Some(first_time)));

        Ok(vec![node])
    }
}

pub fn build_replay(
    path: PathBuf,
    speed: f32,
    _registry: Arc<ChannelRegistry>,
    denylist: HashSet<ChannelName>,
    stop_signal_cell: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
) -> Result<(LiveReplayConfig, ReplayBuildStep), Box<dyn std::error::Error + Send + Sync>> {
    let mut log_reader = SortedLogStreamReader::from_path(&path, 65536)?;

    if log_reader.is_empty() {
        return Err("Log file is empty".into());
    }

    let first_log_time = log_reader
        .peek_time()
        .unwrap_or(FrameworkTime::from_nanoseconds(0));

    let config = LiveReplayConfig {
        replay_speed: speed,
        first_log_time,
        paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let build_step = ReplayBuildStep {
        reader: Arc::new(Mutex::new(Some(log_reader))),
        first_time: first_log_time,
        denylist,
        stop_signal_cell,
    };

    Ok((config, build_step))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};

    use logging::log_file::LogFileWriter;
    use logging::log_file_json::JsonLogFileWriter;
    use logging::sorted_log_stream::SortedLogStreamReader;
    use task::callback::CallbackViews;
    use task::channel_registry::ChannelRegistry;
    use task::context::Context;
    use task::executor::ExecutorStopSignal;
    use task::input::OptionalInput;
    use task::publisher::Publisher;
    use task::string_interner::{CallbackNameInterner, ChannelNameInterner};
    use task::subscriber::{Subscriber, SubscriberConfig};
    use task::time::FrameworkTime;

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
    fn test_replay_task_publishes_u64_driven_by_time() {
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
        }

        let stop_signal_cell = Arc::new(OnceLock::new());
        let _ = stop_signal_cell
            .set(Arc::new(TestStopSignal(stopped.clone())) as Arc<dyn ExecutorStopSignal>);

        let mut reader = SortedLogStreamReader::from_reader(log_buf.as_slice(), 64).unwrap();
        let first_time = reader.peek_time().unwrap();

        let build_step = ReplayBuildStep {
            reader: Arc::new(Mutex::new(Some(reader))),
            first_time,
            denylist: HashSet::new(),
            stop_signal_cell: stop_signal_cell.clone(),
        };

        let mut channel_registry = ChannelRegistry::new();
        channel_registry.register_channel::<u64>("integer".into());
        let nodes = build_step.build_step(&[], &mut channel_registry).unwrap();
        assert_eq!(nodes.len(), 1);
        let mut node = nodes.into_iter().next().unwrap();
        assert_eq!(node.name(), "ReplayTask");

        // Create a subscriber on "integer" to observe published messages
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

        let channel_interner = ChannelNameInterner::default();
        let callback_interner = CallbackNameInterner::default();

        // Time 500: nothing due yet
        let t500 = FrameworkTime::from_nanoseconds(500);
        node.run(&Context::new(t500, &channel_interner, &callback_interner));
        node.flush_publishers(t500);
        sub.drain_writer_to_reader();
        {
            let input = OptionalInput::<u64>::new_downcasted(&mut sub);
            assert!(input.value().is_none());
        }

        // Time 1000: first entry should publish
        let t1000 = FrameworkTime::from_nanoseconds(1_000);
        node.run(&Context::new(t1000, &channel_interner, &callback_interner));
        node.flush_publishers(t1000);
        sub.drain_writer_to_reader();
        {
            let mut input = OptionalInput::<u64>::new_downcasted(&mut sub);
            assert_eq!(input.value(), Some(&42u64));
            input.clear();
            assert!(input.value().is_none());
        }

        // Time 2000: cursor behind second entry — nothing new
        let t2000 = FrameworkTime::from_nanoseconds(2_000);
        node.run(&Context::new(t2000, &channel_interner, &callback_interner));
        node.flush_publishers(t2000);
        sub.drain_writer_to_reader();
        {
            let input = OptionalInput::<u64>::new_downcasted(&mut sub);
            assert!(input.value().is_none());
        }

        // Time 3000: second entry should publish, then stop
        let t3000 = FrameworkTime::from_nanoseconds(3_000);
        node.run(&Context::new(t3000, &channel_interner, &callback_interner));
        node.flush_publishers(t3000);
        sub.drain_writer_to_reader();
        {
            let mut input = OptionalInput::<u64>::new_downcasted(&mut sub);
            assert_eq!(input.value(), Some(&7u64));
            input.clear();
            assert!(input.value().is_none());
        }
        assert!(stopped.load(Ordering::Relaxed));
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "Miri doesn't support file I/O: SortedLogStreamReader::from_reader copies its input to a temp file"
    )]
    fn test_replay_task_drylists_execution_log() {
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
        let mut reader = SortedLogStreamReader::from_reader(log_buf.as_slice(), 64).unwrap();
        let first_time = reader.peek_time().unwrap();

        let mut channel_registry = ChannelRegistry::new();
        channel_registry.register_channel::<u64>("integer".into());

        let build_step = ReplayBuildStep {
            reader: Arc::new(Mutex::new(Some(reader))),
            first_time,
            denylist: HashSet::new(),
            stop_signal_cell,
        };

        let nodes = build_step.build_step(&[], &mut channel_registry).unwrap();
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
    fn test_drylised_channels_skipped() {
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
        let mut reader = SortedLogStreamReader::from_reader(log_buf.as_slice(), 64).unwrap();
        let first_time = reader.peek_time().unwrap();

        let mut denylist = HashSet::new();
        denylist.insert("integer".to_string());

        let mut channel_registry = ChannelRegistry::new();
        channel_registry.register_channel::<u64>("integer".into());

        let build_step = ReplayBuildStep {
            reader: Arc::new(Mutex::new(Some(reader))),
            first_time,
            denylist,
            stop_signal_cell,
        };

        let nodes = build_step.build_step(&[], &mut channel_registry).unwrap();
        assert!(nodes.is_empty());
    }
}
