#![cfg(feature = "iceoryx2")]
use exact_replay_executor::{ExactReplayExecutor, ReplayBindings, ReplayLog};
use iceoryx2::prelude::ZeroCopySend;
use logging::{Capture, ExecutionRecorder, LogFileWriter, LogSession, PortCapture, ReplaySource};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use task::iox2::{
    Iox2ChannelConfig, Iox2ChannelPlan, Iox2Event, Iox2NotifyOutput, Iox2OptionalInput, Iox2Output,
    Iox2Runtime, Iox2SpanInput,
};
use task::{Callback, GraphBuilder, GraphPlan, message::MessageHeader, time::FrameworkTime};
use task_macros::task_callback;

#[derive(Clone, Default)]
struct Bytes(Arc<Mutex<Vec<u8>>>);
impl Write for Bytes {
    fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(value);
        Ok(value.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn at(n: i64) -> FrameworkTime {
    FrameworkTime::from_nanoseconds(n)
}
fn input_header(publisher_index: u32, batch_index: u32) -> MessageHeader {
    MessageHeader {
        published_at: at(1),
        publisher_index,
        batch_index,
    }
}
#[repr(C)]
#[derive(Debug, Default, serde::Serialize, serde::Deserialize, ZeroCopySend)]
struct Value {
    value: u64,
}
struct Observe;
#[task_callback]
impl Observe {
    fn run(
        &self,
        first: Iox2OptionalInput<u64>,
        second: Iox2OptionalInput<u64>,
        event: Iox2Event,
        mut output: Iox2Output<Value>,
        notify: Iox2NotifyOutput,
    ) {
        assert_eq!(
            event.records().collect::<Vec<_>>(),
            [(iceoryx2::prelude::EventId::new(9), 5)]
        );
        assert_eq!(*first.header().unwrap(), input_header(3, 0));
        assert_eq!(*second.header().unwrap(), input_header(4, 1));
        output.value = first.value().unwrap() + second.value().unwrap() + event.count();
        output.send();
        notify.send();
    }
}
#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn ipc_inputs_are_endpoint_local_events_are_restored_once_and_outputs_stay_local() {
    let bytes = Bytes::default();
    let runtime = Iox2Runtime::new().unwrap();
    let input_name = format!("exact_input_{}", std::process::id());
    let output_name = format!("exact_output_{}", std::process::id());
    let notify_name = format!("exact_notify_{}", std::process::id());
    {
        let mut input = Iox2ChannelPlan::new(&input_name, &runtime);
        let mut output = Iox2ChannelPlan::new(&output_name, &runtime);
        output.publisher(1); // Unused storage precedes the recorded output port.
        let mut notify =
            Iox2ChannelPlan::<()>::new(&notify_name, &runtime).with_config(Iox2ChannelConfig {
                event_id_max_value: 17,
                ..Default::default()
            });
        let declaration = ObserveDeclaration::from_keys(
            input.subscriber(1),
            input.subscriber(1),
            input.events(4),
            output.publisher(1),
            notify.notifier_with_id(17),
        );
        let capture_input = input.subscriber(1);
        let capture_output = output.subscriber(1);
        let storage = GraphPlan::new((input, (output, notify)))
            .allocate()
            .unwrap();
        let input = storage.channels().0.build().unwrap();
        let output = storage.channels().1.0.build().unwrap();
        let notify = storage.channels().1.1.build().unwrap();
        input
            .replay_input(declaration.first_key())
            .unwrap()
            .inject(input_header(3, 0), 7)
            .unwrap();
        input
            .replay_input(declaration.second_key())
            .unwrap()
            .inject(input_header(4, 1), 11)
            .unwrap();
        let mut callback = Observe
            .bind(declaration, &input, &input, &input, &output, &notify)
            .unwrap();
        callback.stage_replay_event(2, 9, 5).unwrap();
        let mut graph = GraphBuilder::with_storage(&storage);
        graph.add_callback("observe", || Ok(callback));
        let recorder = ExecutionRecorder::new(2);
        let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
        let mut writer = logging::log_file_json::JsonLogFileWriter::new(bytes.clone());
        writer
            .store_message(&input_name, &input_header(3, 0), b"7")
            .unwrap();
        writer
            .store_message(&input_name, &input_header(4, 1), b"11")
            .unwrap();
        let observed = task::recording::ObservedEvent {
            callback_id: task::string_interner::CallbackId::from_index(0).unwrap(),
            observed_at: at(5),
            event: task::recording::LoggedEvent {
                ordinal: 2,
                event_id: 9,
                count: 5,
            },
        };
        writer
            .store_message(
                task::recording::EXECUTION_EVENT_CHANNEL,
                &MessageHeader::new(at(5)),
                &serde_json::to_vec(&observed).unwrap(),
            )
            .unwrap();
        let log = LogSession::new(
            writer,
            vec![
                Capture::ipc(input.take_subscriber(&capture_input).unwrap()),
                Capture::ipc(output.take_subscriber(&capture_output).unwrap()),
            ],
        )
        .with_recording(recorder)
        .unwrap();
        graph.step(at(10)).unwrap();
        log.finish().unwrap();
    }
    let log = ReplayLog::from_reader(
        &logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap(),
    )
    .unwrap();
    let mut input = Iox2ChannelPlan::new(&input_name, &runtime);
    let mut output = Iox2ChannelPlan::new(&output_name, &runtime);
    let mut notify =
        Iox2ChannelPlan::<()>::new(&notify_name, &runtime).with_config(Iox2ChannelConfig {
            event_id_max_value: 17,
            ..Default::default()
        });
    let declaration = ObserveDeclaration::from_keys(
        input.subscriber(1),
        input.subscriber(1),
        input.events(4),
        output.publisher(1),
        notify.notifier_with_id(17),
    );
    let probe = output.subscriber(1);
    let storage = GraphPlan::new((input, (output, notify)))
        .allocate()
        .unwrap();
    let input = storage.channels().0.build().unwrap();
    let output = storage.channels().1.0.build().unwrap();
    let notify = storage.channels().1.1.build().unwrap();
    let probe = output.take_subscriber(&probe).unwrap();
    let mut ports = ReplayBindings::new();
    ports
        .add_input(
            "observe",
            0,
            ReplaySource::ipc_input(input.replay_input(declaration.first_key()).unwrap()),
        )
        .unwrap();
    ports
        .add_input(
            "observe",
            1,
            ReplaySource::ipc_input(input.replay_input(declaration.second_key()).unwrap()),
        )
        .unwrap();
    ports
        .add_output(
            "observe",
            0,
            output
                .configure_publisher(declaration.output_key(), |p| PortCapture::ipc(p, 2))
                .unwrap(),
        )
        .unwrap();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_callback("observe", || {
        Ok(Observe.bind(declaration, &input, &input, &input, &output, &notify)?)
    });
    assert!(
        ExactReplayExecutor::new(graph.build().unwrap(), log, ports)
            .unwrap()
            .run()
            .unwrap()
            .is_exact()
    );
    probe.update();
    assert!(
        Iox2SpanInput::new(&probe).is_empty(),
        "exact replay must not emit live IPC data"
    );
}
