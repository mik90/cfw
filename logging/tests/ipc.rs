#![cfg(all(feature = "serde", feature = "iceoryx2"))]
use iceoryx2::prelude::{EventId, ZeroCopySend};
use logging::{Capture, ExecutionRecorder, ReplaySource};
use std::time::Duration;
use task::iox2::{
    Iox2ChannelPlan, Iox2Event, Iox2NotifyOutput, Iox2OptionalInput, Iox2Output, Iox2Runtime,
};
use task::{
    CallbackSchedule, ChannelPlan, GraphBuilder, GraphPlan, RequiredInput,
    message::MessageHeader,
    recording::{Outcome, Transport},
    time::FrameworkTime,
};
use task_macros::task_callback;

#[repr(C)]
#[derive(Debug, Default, serde::Serialize, serde::Deserialize, ZeroCopySend)]
struct Payload {
    value: u64,
}
struct Observe;
#[task_callback]
impl Observe {
    fn run(
        &self,
        input: Iox2OptionalInput<u64>,
        event: Iox2Event,
        #[trigger(false)] gate: RequiredInput<u64>,
        mut output: Iox2Output<Payload>,
        notify: Iox2NotifyOutput,
    ) {
        assert_eq!(event.count(), 5);
        output.value = input.value().unwrap() + *gate;
        output.send();
        notify.send();
    }
}
#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn recording_preserves_counted_events_gated_inputs_and_output_notifier_ids() {
    let runtime = Iox2Runtime::new().unwrap();
    let name = format!("recording_input_{}", std::process::id());
    let mut input = Iox2ChannelPlan::new(&name, &runtime);
    let mut output =
        Iox2ChannelPlan::new(format!("recording_output_{}", std::process::id()), &runtime);
    let mut events =
        Iox2ChannelPlan::<()>::new(format!("recording_events_{}", std::process::id()), &runtime)
            .with_config(task::iox2::Iox2ChannelConfig {
                event_id_max_value: 17,
                ..Default::default()
            });
    let mut gate = ChannelPlan::new("gate");
    let declaration = ObserveDeclaration::from_keys(
        input.subscriber(1),
        input.events(4),
        gate.subscriber(1),
        output.publisher(1),
        events.notifier_with_id(17),
    );
    let injection = input.publisher(1);
    let gate_key = gate.publisher(1);
    let capture = output.subscriber(1);
    let storage = GraphPlan::new((input, (output, (events, gate))))
        .allocate()
        .unwrap();
    let input = storage.channels().0.build().unwrap();
    let output = storage.channels().1.0.build().unwrap();
    let events = storage.channels().1.1.0.build().unwrap();
    let gate = storage.channels().1.1.1.build();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_scheduled_callback(
        "observe",
        CallbackSchedule::default().with_execution_duration(Duration::from_nanos(2)),
        || Ok(Observe.bind(declaration, &input, &input, &gate, &output, &events)?),
    );
    let recorder = ExecutionRecorder::new(2);
    let graph = recorder.attach(graph.build().unwrap()).unwrap();
    let mut simulation = simulation_executor::SimulationState::with_config(
        graph,
        simulation_executor::SimulationConfig {
            start_time: FrameworkTime::from_nanoseconds(10),
            ..Default::default()
        },
    )
    .unwrap();
    let mut replay = ReplaySource::ipc(input.take_publisher(&injection).unwrap());
    replay
        .inject(
            MessageHeader::new(FrameworkTime::from_nanoseconds(7)),
            b"42",
        )
        .unwrap();
    simulation
        .schedule_event(
            FrameworkTime::from_nanoseconds(10),
            &name,
            EventId::new(9),
            5,
        )
        .unwrap();
    assert!(simulation.step().unwrap().executed.is_empty());
    assert!(
        recorder.drain().is_empty(),
        "gated callbacks did not execute"
    );
    #[derive(Clone, Default)]
    struct Bytes(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Bytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Bytes::default();
    let mut log = logging::LogSession::new(
        logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
        vec![],
    )
    .with_recording(recorder.clone())
    .unwrap();
    log.flush().unwrap();
    let reader =
        logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap();
    use logging::LogFileReader;
    let entry = reader.entry(0).unwrap();
    assert_eq!(entry.channel_name, task::recording::EXECUTION_EVENT_CHANNEL);
    let activation: logging::ObservedEvent = serde_json::from_slice(entry.serialized_body).unwrap();
    assert_eq!(activation.observed_at.to_nanoseconds(), 10);
    assert_eq!(
        (
            activation.callback_index,
            activation.event.ordinal,
            activation.event.count
        ),
        (0, 1, 5)
    );
    let mut publisher = gate.take_publisher(&gate_key).unwrap();
    simulation
        .schedule_at(FrameworkTime::from_nanoseconds(20), move |time| {
            publisher.publish(3).map_err(|e| format!("{e:?}"))?;
            publisher.flush(time);
            Ok(())
        })
        .unwrap();
    assert!(simulation.step().unwrap().executed.is_empty());
    assert_eq!(simulation.step().unwrap().executed, [0]);
    let records = recorder.drain();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.outcome, Outcome::Committed);
    assert_eq!(record.execution_time.to_nanoseconds(), 20);
    assert_eq!(
        record
            .inputs
            .iter()
            .map(|m| (m.ordinal, m.header.published_at.to_nanoseconds()))
            .collect::<Vec<_>>(),
        [(0, 7), (2, 20)]
    );
    assert_eq!(
        (
            record.events[0].ordinal,
            record.events[0].event_id,
            record.events[0].count
        ),
        (1, 9, 5)
    );
    assert_eq!(
        record
            .output_events
            .iter()
            .map(|e| (e.ordinal, e.event_id, e.count))
            .collect::<Vec<_>>(),
        [(0, 0, 1), (1, 17, 1)]
    );
    assert_eq!(
        recorder.descriptor().unwrap().callbacks[0].endpoints[1].transport,
        Transport::Event
    );
    let messages = Capture::ipc(output.take_subscriber(&capture).unwrap())
        .drain_to_vec()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Payload>(&messages[0].1)
            .unwrap()
            .value,
        45
    );
    assert_eq!(messages[0].0.published_at.to_nanoseconds(), 20);
    assert!(simulation.step().unwrap().executed.is_empty());
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn live_readiness_records_arrivals_using_the_injected_clock() {
    use task::iox2::Iox2EventBindings;
    struct Fixed;
    impl task::executor::TimeSource for Fixed {
        fn now(&self) -> FrameworkTime {
            FrameworkTime::from_nanoseconds(100)
        }
    }
    let runtime = Iox2Runtime::new().unwrap();
    let mut input = Iox2ChannelPlan::new(
        format!("recording_live_input_{}", std::process::id()),
        &runtime,
    )
    .with_config(task::iox2::Iox2ChannelConfig {
        event_id_max_value: 9,
        ..Default::default()
    });
    let mut output = Iox2ChannelPlan::new(
        format!("recording_live_output_{}", std::process::id()),
        &runtime,
    );
    let mut events = Iox2ChannelPlan::<()>::new(
        format!("recording_live_events_{}", std::process::id()),
        &runtime,
    )
    .with_config(task::iox2::Iox2ChannelConfig {
        event_id_max_value: 17,
        ..Default::default()
    });
    let mut gate = ChannelPlan::new("gate");
    let declaration = ObserveDeclaration::from_keys(
        input.subscriber(1),
        input.events(4),
        gate.subscriber(1),
        output.publisher(1),
        events.notifier_with_id(17),
    );
    let data_key = input.publisher(1);
    let event_key = input.notifier_with_id(9);
    let gate_key = gate.publisher(1);
    let storage = GraphPlan::new((input, (output, (events, gate))))
        .allocate()
        .unwrap();
    let input = storage.channels().0.build().unwrap();
    let output = storage.channels().1.0.build().unwrap();
    let events = storage.channels().1.1.0.build().unwrap();
    let gate = storage.channels().1.1.1.build();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_callback("observe", || {
        Ok(Observe.bind(declaration, &input, &input, &gate, &output, &events)?)
    });
    let recorder = ExecutionRecorder::new(4);
    let graph = recorder.attach(graph.build().unwrap()).unwrap();
    ReplaySource::ipc(input.take_publisher(&data_key).unwrap())
        .inject(
            MessageHeader::new(FrameworkTime::from_nanoseconds(7)),
            b"42",
        )
        .unwrap();
    let mut publisher = gate.take_publisher(&gate_key).unwrap();
    publisher.publish(3).unwrap();
    publisher.flush(FrameworkTime::from_nanoseconds(7));
    let mut notifier = input.take_notifier(&event_key).unwrap();
    for _ in 0..5 {
        Iox2NotifyOutput::new(&mut notifier).send();
        notifier.flush(FrameworkTime::from_nanoseconds(7));
    }
    let executor =
        live_executor::LiveExecutor::new_multi_pool_with_time(vec![1], graph, Fixed).unwrap();
    let records = executor
        .run_with(|stop| {
            let start = std::time::Instant::now();
            loop {
                let records = recorder.drain();
                if !records.is_empty() {
                    stop.request_stop();
                    break records;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "callback never executed"
                );
                std::thread::yield_now();
            }
        })
        .unwrap();
    assert_eq!(records[0].outcome, Outcome::Committed);
    let arrivals = recorder.drain_events();
    assert_eq!(arrivals.len(), 1);
    assert_eq!(
        arrivals[0].observed_at,
        FrameworkTime::from_nanoseconds(100)
    );
    assert_eq!(
        (
            arrivals[0].event.ordinal,
            arrivals[0].event.event_id,
            arrivals[0].event.count
        ),
        (1, 9, 5)
    );
}
