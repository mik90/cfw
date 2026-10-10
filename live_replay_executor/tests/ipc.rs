#![cfg(feature = "iceoryx2")]
use live_replay_executor::{LiveReplayConfig, LiveReplayExecutor};
use logging::{CapturePlan, OwnedLogEntry, ReplaySource, ReplaySourcePlan, SortedLogStreamReader};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use task::iox2::{Iox2ChannelPlan, Iox2Event, Iox2OptionalInput, Iox2Output, Iox2Runtime};
use task::{
    ChannelPlan, GraphBuilder, GraphPlan, LoanError, Publisher, RequiredInput,
    message::MessageHeader, recording::*, time::FrameworkTime,
};
use task_macros::task_callback;

struct Producer;
#[task_callback]
impl Producer {
    fn run(&self, input: RequiredInput<u64>, mut output: Iox2Output<u64>) {
        std::thread::sleep(Duration::from_millis(5));
        *output = *input * 2;
        output.send();
    }
}
type Trace = Arc<Mutex<Vec<(&'static str, u64)>>>;
struct Receive {
    name: &'static str,
    trace: Trace,
}
#[task_callback]
impl Receive {
    fn run(
        &self,
        event: Iox2Event,
        input: Iox2OptionalInput<u64>,
        output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        self.trace.lock().unwrap().push((self.name, event.count()));
        output.publish(input.value().unwrap() + event.count())
    }
}
fn row(time: i64, channel: &str, bytes: &[u8]) -> OwnedLogEntry {
    OwnedLogEntry {
        header: MessageHeader::new(FrameworkTime::from_nanoseconds(time)),
        channel_name: channel.into(),
        serialized_body: bytes.into(),
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn eof_drain_waits_for_computed_ipc_notifications_and_downstream_callbacks() {
    let runtime = Iox2Runtime::new().unwrap();
    let mut input = ChannelPlan::new("input");
    let mut middle = Iox2ChannelPlan::new(
        format!("live_replay_middle_{}", std::process::id()),
        &runtime,
    );
    let mut output = ChannelPlan::new("output");
    let a = Producer::declare(&mut input, &mut middle).unwrap();
    let b =
        ReceiveDeclaration::from_keys(middle.events(4), middle.subscriber(1), output.publisher(1));
    let source = ReplaySourcePlan::declare(&mut input, 1);
    let capture = CapturePlan::declare(&mut output, 1);
    let storage = GraphPlan::new((input, (middle, output)))
        .allocate()
        .unwrap();
    let input = storage.channels().0.build();
    let middle = storage.channels().1.0.build().unwrap();
    let output = storage.channels().1.1.build();
    let trace: Trace = Default::default();
    let mut graph = GraphBuilder::new();
    graph.add_callback("producer", || Ok(Producer.bind(a, &input, &middle)?));
    graph.add_callback("receive", || {
        Ok(Receive {
            name: "receive",
            trace: trace.clone(),
        }
        .bind(b, &middle, &middle, &output)?)
    });
    let reader =
        SortedLogStreamReader::from_entries(vec![row(0, "input", b"21")], HashMap::new()).unwrap();
    let completion = LiveReplayExecutor::new(
        graph.build().unwrap(),
        reader,
        [source.bind(&input).unwrap()],
        LiveReplayConfig::default(),
    )
    .unwrap()
    .run()
    .unwrap();
    assert!(completion.drained);
    assert_eq!(*trace.lock().unwrap(), [("receive", 1)]);
    assert_eq!(
        capture.bind(&output).unwrap().drain_to_vec().unwrap()[0].1,
        b"43"
    );
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn recorded_events_target_recipients_without_duplicate_kernel_notifications() {
    let runtime = Iox2Runtime::new().unwrap();
    let channel = format!("live_replay_injection_{}", std::process::id());
    let mut input = Iox2ChannelPlan::<u64>::new(&channel, &runtime);
    let mut output = ChannelPlan::new("output");
    let a =
        ReceiveDeclaration::from_keys(input.events(4), input.subscriber(1), output.publisher(1));
    let b =
        ReceiveDeclaration::from_keys(input.events(4), input.subscriber(1), output.publisher(1));
    let source = input.publisher(1);
    let storage = GraphPlan::new((input, output)).allocate().unwrap();
    let input = storage.channels().0.build().unwrap();
    let output = storage.channels().1.build();
    let trace: Trace = Default::default();
    let mut graph = GraphBuilder::new();
    graph.add_callback("b", || {
        Ok(Receive {
            name: "b",
            trace: trace.clone(),
        }
        .bind(b, &input, &input, &output)?)
    });
    graph.add_callback("a", || {
        Ok(Receive {
            name: "a",
            trace: trace.clone(),
        }
        .bind(a, &input, &input, &output)?)
    });
    let descriptor = ExecutionDescriptor {
        callbacks: ["a", "b"]
            .into_iter()
            .map(|name| CallbackDescriptor {
                name: name.into(),
                endpoints: vec![EndpointDescriptor {
                    ordinal: 0,
                    channel: channel.clone(),
                    direction: Direction::Received,
                    transport: Transport::Event,
                    payload_type: "()".into(),
                    publisher_index: None,
                }],
            })
            .collect(),
        logged_channels: vec![channel.clone()],
    };
    let mut rows = vec![row(0, &channel, b"42")];
    for (index, count) in [(0, 3), (1, 5)] {
        let event = ObservedEvent {
            callback_index: index,
            observed_at: FrameworkTime::from_nanoseconds(0),
            event: LoggedEvent {
                ordinal: 0,
                event_id: 0,
                count,
            },
        };
        rows.push(row(
            0,
            EXECUTION_EVENT_CHANNEL,
            &serde_json::to_vec(&event).unwrap(),
        ));
    }
    let reader = SortedLogStreamReader::from_entries(
        rows,
        HashMap::from([(
            EXECUTION_LOG_DESCRIPTOR_ARTIFACT.into(),
            serde_json::to_vec(&descriptor).unwrap(),
        )]),
    )
    .unwrap();
    let completion = LiveReplayExecutor::new(
        graph.build().unwrap(),
        reader,
        [ReplaySource::ipc(input.take_publisher(&source).unwrap())],
        LiveReplayConfig::default(),
    )
    .unwrap()
    .run()
    .unwrap();
    assert!(completion.drained);
    let mut actual = trace.lock().unwrap().clone();
    actual.sort();
    assert_eq!(actual, [("a", 3), ("b", 5)]);
}
