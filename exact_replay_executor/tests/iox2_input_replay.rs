#![cfg(feature = "iceoryx2")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use exact_replay_executor::{ExactReplayConfig, ExactReplayExecutor};
use logging::log_file::LogFileWriter;
use logging::log_file_json::{JsonLogFileReader, JsonLogFileWriter};
use task::callback::{Callback, PubOrSub, PubOrSubMut};
use task::channel_registry::ChannelRegistry;
use task::context::Context;
use task::execution_log::{
    Direction, EXECUTION_LOG_CHANNEL, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, ExecutionLogDescriptor,
    ExecutionLogEntry, ExecutionLogMessage, LoggedIox2Event, LoggedMessage,
};
use task::executor::{Executor, ExecutorParams};
use task::iox2::{Iox2Event, Iox2EventSubscriber, Iox2OptionalInput, Iox2Subscriber};
use task::loggable::Loggable;
use task::message::MessageHeader;
use task::subscriber::SubscriberConfig;
use task::task_graph_builder::TaskGraphBuilder;
use task::time::FrameworkTime;

type Observation = (
    FrameworkTime,
    Option<(u64, FrameworkTime)>,
    Vec<(usize, u64)>,
);
type EventObservations = Arc<Mutex<Vec<Vec<(usize, u64)>>>>;

struct Iox2InputCallback {
    data: Iox2Subscriber<u64>,
    events: Iox2EventSubscriber,
    observed: Arc<Mutex<Vec<Observation>>>,
}

struct EventOnlyCallback {
    input: Iox2EventSubscriber,
    observed: EventObservations,
}

impl Callback for EventOnlyCallback {
    fn run(&mut self, _ctx: &Context) {
        let events = Iox2Event::new(&self.input)
            .records()
            .map(|(id, count)| (id.as_value(), count))
            .collect();
        self.observed.lock().unwrap().push(events);
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Subscriber(&self.input));
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Subscriber(&mut self.input));
    }
}

impl Callback for Iox2InputCallback {
    fn run(&mut self, ctx: &Context) {
        let data = Iox2OptionalInput::new(&self.data);
        let value = data
            .value()
            .copied()
            .zip(data.header().map(|header| header.published_at));
        drop(data);
        let events = Iox2Event::new(&self.events)
            .records()
            .map(|(id, count)| (id.as_value(), count))
            .collect();
        self.observed.lock().unwrap().push((ctx.now, value, events));
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Subscriber(&self.data));
        f(PubOrSub::Subscriber(&self.events));
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Subscriber(&mut self.data));
        f(PubOrSubMut::Subscriber(&mut self.events));
    }
}

fn input_config(channel: &str, trigger: bool) -> SubscriberConfig {
    SubscriberConfig {
        is_optional: true,
        capacity: 4,
        is_trigger: trigger,
        keep_across_runs: true,
        channel_name: channel.into(),
    }
}

fn input_node(
    data_channel: &str,
    event_channel: &str,
    observed: Arc<Mutex<Vec<Observation>>>,
) -> task::callback::CallbackNode {
    task::callback::CallbackNode::new_named(
        Box::new(Iox2InputCallback {
            data: Iox2Subscriber::new(input_config(data_channel, false)),
            events: Iox2EventSubscriber::new(input_config(event_channel, true)),
            observed,
        }),
        "iox2_exact_consumer".into(),
    )
}

fn write_log(
    descriptor: &ExecutionLogDescriptor,
    data_channel: &str,
    header: MessageHeader,
    payload: u64,
) -> JsonLogFileReader {
    let event = ExecutionLogEntry {
        callback_node_index: 0,
        execution_time: FrameworkTime::from_nanoseconds(50),
        iox2_event: Some(LoggedIox2Event {
            subscriber_ordinal: 1,
            event_id: 7,
            count: 3,
            observed_at: FrameworkTime::from_nanoseconds(50),
        }),
        ..Default::default()
    };
    let mut execution = ExecutionLogEntry {
        callback_node_index: 0,
        execution_time: FrameworkTime::from_nanoseconds(100),
        log_whole: true,
        ..Default::default()
    };
    execution.messages[0] = LoggedMessage {
        ordinal: 0,
        direction: Direction::Received,
        header,
    };
    let mut batch = ExecutionLogMessage::default();
    batch.entries[0] = event;
    batch.entries[1] = execution;

    let mut bytes = Vec::new();
    {
        let mut writer = JsonLogFileWriter::new(&mut bytes);
        writer
            .write_artifact(
                EXECUTION_LOG_DESCRIPTOR_ARTIFACT,
                &serde_json::to_vec(descriptor).unwrap(),
            )
            .unwrap();
        let mut body = Vec::new();
        payload.serialize(&mut body).unwrap();
        writer.store_message(data_channel, &header, &body).unwrap();
        let mut log_body = Vec::new();
        batch.serialize(&mut log_body).unwrap();
        writer
            .store_message(
                EXECUTION_LOG_CHANNEL,
                &MessageHeader::new(FrameworkTime::from_nanoseconds(200)),
                &log_body,
            )
            .unwrap();
    }
    JsonLogFileReader::from_reader(bytes.as_slice()).unwrap()
}

#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn exact_replay_hydrates_iox2_event_and_data_inputs() {
    let data_channel = format!("exact_iox2_data_{}", std::process::id());
    let event_channel = format!("exact_iox2_event_{}", std::process::id());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let node = input_node(&data_channel, &event_channel, Arc::clone(&observed));
    let descriptor = ExecutionLogDescriptor::new(std::slice::from_ref(&node));
    let mut graph = TaskGraphBuilder::new()
        .add_pool(1, |pool| pool.add_callback(node))
        .build()
        .unwrap();
    let data_header = MessageHeader::new(FrameworkTime::from_nanoseconds(40));
    let reader = write_log(&descriptor, &data_channel, data_header, 42);
    let mut registry = ChannelRegistry::new();
    registry.register_channel::<u64>(data_channel);
    let params = ExecutorParams::new(std::mem::take(&mut graph.pools))
        .with_iox2_context(graph.iox2_context.take());
    let config = ExactReplayConfig::new(params, registry, Box::new(reader));
    let mut executor = ExactReplayExecutor::new(config).unwrap();
    executor.start();
    let deadline = Instant::now() + Duration::from_secs(3);
    while executor.is_running() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(!executor.is_running(), "exact replay did not finish");
    let result = executor.stop();
    assert!(
        result.is_ok(),
        "exact replay errors: {:?}",
        executor.replay_errors()
    );
    assert_eq!(executor.consumed_count(), 1);
    assert_eq!(
        *observed.lock().unwrap(),
        vec![(
            FrameworkTime::from_nanoseconds(100),
            Some((42, FrameworkTime::from_nanoseconds(40))),
            vec![(7, 3)]
        )]
    );
}

#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn exact_replay_names_missing_iox2_graph_context() {
    let data_channel = format!("exact_missing_context_{}", std::process::id());
    let event_channel = format!("exact_missing_event_{}", std::process::id());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let node = input_node(&data_channel, &event_channel, Arc::clone(&observed));
    let descriptor = ExecutionLogDescriptor::new(std::slice::from_ref(&node));
    let mut graph = TaskGraphBuilder::new()
        .add_pool(1, |pool| pool.add_callback(node))
        .build()
        .unwrap();
    // Keep the graph's node alive through this test, but do not attach its
    // context to the executor so hydration reports the missing dependency.
    let context = graph.iox2_context.take();
    let params = ExecutorParams::new(std::mem::take(&mut graph.pools));
    let reader = write_log(
        &descriptor,
        &data_channel,
        MessageHeader::new(FrameworkTime::from_nanoseconds(40)),
        42,
    );
    let mut registry = ChannelRegistry::new();
    registry.register_channel::<u64>(data_channel.clone());
    let mut executor =
        ExactReplayExecutor::new(ExactReplayConfig::new(params, registry, Box::new(reader)))
            .unwrap();
    executor.start();
    let deadline = Instant::now() + Duration::from_secs(3);
    while executor.is_running() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(!executor.is_running());
    let result = executor.stop();
    assert!(result.is_err());
    assert!(
        executor
            .replay_errors()
            .iter()
            .any(|error| error.to_string().contains(&data_channel)
                && error.to_string().contains("context"))
    );
    assert!(observed.lock().unwrap().is_empty());
    drop(executor);
    drop(context);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "serde_json log parsing is prohibitively slow under Miri"
)]
fn exact_replay_stages_each_event_before_its_recorded_execution() {
    let channel = format!("exact_event_only_{}", std::process::id());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let node = task::callback::CallbackNode::new_named(
        Box::new(EventOnlyCallback {
            input: Iox2EventSubscriber::new(input_config(&channel, true)),
            observed: Arc::clone(&observed),
        }),
        "event_only_exact_consumer".into(),
    );
    let descriptor = ExecutionLogDescriptor::new(std::slice::from_ref(&node));
    let mut batch = ExecutionLogMessage::default();
    for (index, (observed_at, event_id, count)) in [(150, 2, 5), (50, 1, 3)].into_iter().enumerate()
    {
        batch.entries[index] = ExecutionLogEntry {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(observed_at),
            iox2_event: Some(LoggedIox2Event {
                subscriber_ordinal: 0,
                event_id,
                count,
                observed_at: FrameworkTime::from_nanoseconds(observed_at),
            }),
            ..Default::default()
        };
    }
    for (index, at) in [100, 200].into_iter().enumerate() {
        batch.entries[index + 2] = ExecutionLogEntry {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(at),
            log_whole: true,
            ..Default::default()
        };
    }
    let mut bytes = Vec::new();
    {
        let mut writer = JsonLogFileWriter::new(&mut bytes);
        writer
            .write_artifact(
                EXECUTION_LOG_DESCRIPTOR_ARTIFACT,
                &serde_json::to_vec(&descriptor).unwrap(),
            )
            .unwrap();
        let mut body = Vec::new();
        batch.serialize(&mut body).unwrap();
        writer
            .store_message(
                EXECUTION_LOG_CHANNEL,
                &MessageHeader::new(FrameworkTime::from_nanoseconds(300)),
                &body,
            )
            .unwrap();
    }
    let reader = JsonLogFileReader::from_reader(bytes.as_slice()).unwrap();
    let params = ExecutorParams::new(vec![task::executor::ThreadPoolConfig::new(1, vec![node])]);
    let mut executor = ExactReplayExecutor::new(ExactReplayConfig::new(
        params,
        ChannelRegistry::new(),
        Box::new(reader),
    ))
    .unwrap();
    executor.start();
    let deadline = Instant::now() + Duration::from_secs(3);
    while executor.is_running() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        !executor.is_running(),
        "event-only exact replay did not finish"
    );
    let result = executor.stop();
    assert!(
        result.is_ok(),
        "event replay errors: {:?}",
        executor.replay_errors()
    );
    assert_eq!(*observed.lock().unwrap(), vec![vec![(1, 3)], vec![(2, 5)]]);
}
