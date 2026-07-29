use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use logging::log_file::LogFileReader;
use logging::log_file_json::JsonLogFileReader;
use task::callback::{Callback, CallbackNode, PortMut, Run};
use task::channel_registry::ChannelRegistry;
use task::context::Context;
use task::execution_log::EXECUTION_LOG_CHANNEL;
use task::executor::ExecutorStopSignal;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::message::MessageHeader;
use task::pub_sub::ChannelName;
use task::task_graph_builder::{TaskGraphBuildStep, TaskGraphBuildStepError};
use task::time::FrameworkTime;

use crate::LiveReplayConfig;

#[derive(Clone, Debug)]
pub(crate) struct OwnedLogEntry {
    pub header: MessageHeader,
    pub channel_name: ChannelName,
    pub serialized_body: Vec<u8>,
}

pub struct ReplayTask {
    entries: Vec<OwnedLogEntry>,
    registry: Arc<ChannelRegistry>,
    channel_to_slot: HashMap<ChannelName, usize>,
    writers: Vec<task::channel_registry::ChannelPublisherWriter>,
    publishers: Vec<Box<dyn GenericPublisher>>,
    cursor: usize,
    stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
}

impl ReplayTask {
    fn next_entry_time(&self) -> Option<FrameworkTime> {
        self.entries.get(self.cursor).map(|e| e.header.published_at)
    }
}

impl Callback for ReplayTask {
    fn run(&mut self, ctx: &Context) -> Run {
        let len = self.entries.len();
        while self.cursor < len {
            let entry = &self.entries[self.cursor];
            if entry.header.published_at > ctx.now {
                break;
            }

            let Some(&slot) = self.channel_to_slot.get(entry.channel_name.as_str()) else {
                self.cursor += 1;
                continue;
            };

            let Some(type_id) = self.registry.channel_type(&entry.channel_name) else {
                self.cursor += 1;
                continue;
            };

            let Some(deserializer) = self.registry.deserializer_for(type_id) else {
                self.cursor += 1;
                continue;
            };

            if let Ok(value) = deserializer(&entry.serialized_body) {
                (self.writers[slot])(&mut *self.publishers[slot], value);
            }

            self.cursor += 1;
        }

        if self.cursor >= len {
            if let Some(signal) = self.stop_signal.get() {
                signal.request_stop();
            }
            Run::new(0)
        } else if self.next_entry_time().is_some() {
            Run::new(1)
        } else {
            Run::new(0)
        }
    }

    fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
    fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
        for p in &self.publishers {
            f(p.as_ref());
        }
    }
    fn for_each_subscriber_mut<'a>(
        &'a mut self,
        _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
    ) {
    }
    fn for_each_publisher_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericPublisher)) {
        for p in self.publishers.iter_mut() {
            f(p.as_mut());
        }
    }
    fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
        for p in self.publishers.iter_mut() {
            f(PortMut::Publisher(p.as_mut()));
        }
    }
}

pub struct ReplayBuildStep {
    entries: Vec<OwnedLogEntry>,
    registry: Arc<ChannelRegistry>,
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
    ) -> Result<Vec<CallbackNode>, TaskGraphBuildStepError> {
        let mut replay_channels: Vec<(ChannelName, std::any::TypeId)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for entry in &self.entries {
            if entry.channel_name == EXECUTION_LOG_CHANNEL {
                continue;
            }
            if self.denylist.contains(&entry.channel_name) {
                continue;
            }
            if !seen.insert(entry.channel_name.clone()) {
                continue;
            }
            let Some(type_id) = self.registry.channel_type(&entry.channel_name) else {
                return Err(format!(
                    "ReplayBuildStep: channel '{}' appears in log but was not registered via ChannelRegistry::register_channel",
                    entry.channel_name
                )
                .into());
            };
            replay_channels.push((entry.channel_name.clone(), type_id));
        }

        if replay_channels.is_empty() {
            return Ok(vec![]);
        }

        let mut publishers: Vec<Box<dyn GenericPublisher>> = Vec::new();
        let mut writers = Vec::new();
        let mut channel_to_slot: HashMap<ChannelName, usize> = HashMap::new();

        for (slot, (channel, type_id)) in replay_channels.iter().enumerate() {
            let Some(factory) = self.registry.channel_publisher_factory(*type_id) else {
                return Err(format!(
                    "ReplayBuildStep: no publisher factory for channel '{}' type {:?}",
                    channel, type_id
                )
                .into());
            };

            let (publisher, writer) = factory(channel.clone());
            publishers.push(publisher);
            writers.push(writer);
            channel_to_slot.insert(channel.clone(), slot);
        }

        let first_entry_time = self
            .entries
            .first()
            .map(|e| e.header.published_at)
            .unwrap_or(FrameworkTime::from_nanoseconds(0));

        let replay_task = ReplayTask {
            entries: self.entries.clone(),
            registry: self.registry.clone(),
            channel_to_slot,
            writers,
            publishers,
            cursor: 0,
            stop_signal: self.stop_signal_cell.clone(),
        };

        let mut node = CallbackNode::new_named(Box::new(replay_task), "ReplayTask".into());
        node.set_execution_duration_callback(Box::new(|| std::time::Duration::ZERO));
        node.set_execution_time_callback(Box::new(move |_now| Some(first_entry_time)));

        Ok(vec![node])
    }
}

pub fn build_replay(
    path: PathBuf,
    speed: f32,
    registry: Arc<ChannelRegistry>,
    denylist: HashSet<ChannelName>,
    stop_signal_cell: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
) -> Result<(LiveReplayConfig, ReplayBuildStep), Box<dyn std::error::Error + Send + Sync>> {
    use std::fs::File;
    use std::io::BufReader;

    let file = File::open(&path)?;
    let reader = BufReader::new(file);
    let mut log_reader = JsonLogFileReader::from_reader(reader)?;
    log_reader.sort_by_time();

    if log_reader.is_empty() {
        return Err("Log file is empty".into());
    }

    let first_log_time = log_reader
        .entry(0)
        .map(|e| e.header.published_at)
        .unwrap_or(FrameworkTime::from_nanoseconds(0));

    let entries: Vec<OwnedLogEntry> = log_reader
        .iter()
        .map(|e| OwnedLogEntry {
            header: e.header,
            channel_name: e.channel_name.to_owned(),
            serialized_body: e.serialized_body.to_vec(),
        })
        .collect();

    let config = LiveReplayConfig {
        replay_speed: speed,
        first_log_time,
        paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let build_step = ReplayBuildStep {
        entries,
        registry,
        denylist,
        stop_signal_cell,
    };

    Ok((config, build_step))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};

    use task::callback::CallbackViews;
    use task::channel_registry::ChannelRegistry; // used in test
    use task::loggable::Loggable;
    use task::context::Context;
    use task::executor::ExecutorStopSignal;
    use task::input::OptionalInput;
    use task::publisher::Publisher;
    use task::subscriber::{Subscriber, SubscriberConfig};
    use task::time::FrameworkTime;

    use super::*;

    struct TestStopSignal(Arc<AtomicBool>);
    impl ExecutorStopSignal for TestStopSignal {
        fn request_stop(&self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn test_replay_task_publishes_u64_driven_by_time() {
        let stopped = Arc::new(AtomicBool::new(false));

        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("integer".into());

        // Serialize two u64 values the same way the logger would
        let log_entry = |ns: i64, val: u64| {
            use task::message::MessageHeader;
            let header = MessageHeader::new(FrameworkTime::from_nanoseconds(ns));
            let mut buf = Vec::new();
            val.serialize(&mut buf).unwrap();
            OwnedLogEntry {
                header,
                channel_name: "integer".into(),
                serialized_body: buf,
            }
        };

        let entries = vec![log_entry(1_000, 42u64), log_entry(3_000, 7u64)];

        let registry = Arc::new(registry);

        // Build the replay step by hand to get a ReplayTask
        let stop_signal_cell = Arc::new(OnceLock::new());
        let _ = stop_signal_cell
            .set(Arc::new(TestStopSignal(stopped.clone())) as Arc<dyn ExecutorStopSignal>);

        let build_step = ReplayBuildStep {
            entries,
            registry: registry.clone(),
            denylist: HashSet::new(),
            stop_signal_cell: stop_signal_cell.clone(),
        };

        let nodes = build_step.build_step(&[]).unwrap();
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

        // Time 500: nothing due yet
        let t500 = FrameworkTime::from_nanoseconds(500);
        node.run(&Context::new(t500));
        node.flush_publishers(t500);
        sub.drain_writer_to_reader();
        {
            let input = OptionalInput::<u64>::new_downcasted(&mut sub);
            assert!(input.value().is_none());
        }

        // Time 1000: first entry should publish
        let t1000 = FrameworkTime::from_nanoseconds(1_000);
        node.run(&Context::new(t1000));
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
        node.run(&Context::new(t2000));
        node.flush_publishers(t2000);
        sub.drain_writer_to_reader();
        {
            let input = OptionalInput::<u64>::new_downcasted(&mut sub);
            assert!(input.value().is_none());
        }

        // Time 3000: second entry should publish, then stop
        let t3000 = FrameworkTime::from_nanoseconds(3_000);
        node.run(&Context::new(t3000));
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
    fn test_replay_task_drylists_execution_log() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("integer".into());
        let registry = Arc::new(registry);

        let log_entry = |ns: i64, channel: &str, val: u64| {
            use task::message::MessageHeader;
            let header = MessageHeader::new(FrameworkTime::from_nanoseconds(ns));
            let mut buf = Vec::new();
            val.serialize(&mut buf).unwrap();
            OwnedLogEntry {
                header,
                channel_name: channel.into(),
                serialized_body: buf,
            }
        };

        // First entry is execution_log (skipped), second is integer
        let entries = vec![
            log_entry(100, EXECUTION_LOG_CHANNEL, 0),
            log_entry(500, "integer", 100u64),
        ];

        let stop_signal_cell = Arc::new(OnceLock::new());
        let build_step = ReplayBuildStep {
            entries,
            registry,
            denylist: HashSet::new(),
            stop_signal_cell,
        };

        let nodes = build_step.build_step(&[]).unwrap();
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
        let registry = Arc::new(registry);

        let entries = vec![OwnedLogEntry {
            header: task::message::MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
            channel_name: "integer".into(),
            serialized_body: vec![],
        }];

        let stop_signal_cell = Arc::new(OnceLock::new());
        let mut denylist = HashSet::new();
        denylist.insert("integer".to_string());

        let build_step = ReplayBuildStep {
            entries,
            registry,
            denylist,
            stop_signal_cell,
        };

        let nodes = build_step.build_step(&[]).unwrap();
        assert!(nodes.is_empty());
    }
}
