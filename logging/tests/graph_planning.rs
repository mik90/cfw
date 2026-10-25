use logging::{CaptureOptions, GraphCapturePlan};
use logging::{GraphReplayPlan, ReplayOptions};
use task::{
    InputSpan, LoanError, OutputSpan, Publisher,
    loggable::{DeserializeError, Loggable, SerializeError},
    time::FrameworkTime,
};
use task_graph::{CallbackSchedule, GraphPlan, TaskRegistration};
use task_macros::task_callback;

struct Value(u64);
impl Loggable for Value {
    type Context<'a> = ();
    fn serialize(&self, writer: &mut dyn std::io::Write) -> Result<(), SerializeError> {
        writer.write_all(&self.0.to_le_bytes())?;
        Ok(())
    }
    fn deserialize_with_ctx<'a>(bytes: &[u8], _: ()) -> Result<Self, DeserializeError>
    where
        Self: 'a,
    {
        Ok(Self(u64::from_le_bytes(bytes.try_into()?)))
    }
}
struct Hidden;
struct Contextual;
impl Loggable for Contextual {
    type Context<'a> = &'a u64;
    fn serialize(&self, writer: &mut dyn std::io::Write) -> Result<(), SerializeError> {
        writer.write_all(b"contextual")?;
        Ok(())
    }
    fn deserialize_with_ctx<'a>(_: &[u8], _: &'a u64) -> Result<Self, DeserializeError>
    where
        Self: 'a,
    {
        Ok(Self)
    }
}
struct Source(u64);
#[task_callback]
impl Source {
    fn run(
        &self,
        #[capacity(2)] output: OutputSpan<Value>,
        hidden: &mut Publisher<Hidden>,
    ) -> Result<(), LoanError> {
        let first = output.loan(Value(self.0))?;
        let second = output.loan(Value(self.0 + 1))?;
        second.send();
        first.send();
        hidden.publish(Hidden)
    }
}
struct Sum;
#[task_callback]
impl Sum {
    fn run(
        &self,
        #[trigger(true)]
        #[capacity(4)]
        input: InputSpan<Value>,
        summary: &mut Publisher<Value>,
    ) -> Result<(), LoanError> {
        summary.publish(Value(input.inputs().map(|m| m.message.0).sum()))
    }
}

#[test]
fn discovers_custom_loggable_channels_deduplicates_and_applies_resolved_exclusions() {
    let mut plan = GraphPlan::default();
    let mut a = TaskRegistration::new("a", Source(10), CallbackSchedule::input_triggered());
    a.output_channel("output", "wire");
    a.register(&mut plan).unwrap();
    let mut b = TaskRegistration::new("b", Source(20), CallbackSchedule::input_triggered());
    b.output_channel("output", "wire");
    b.register(&mut plan).unwrap();
    let mut sum = TaskRegistration::new("sum", Sum, CallbackSchedule::input_triggered());
    sum.input_channel("input", "wire")
        .output_channel("summary", "totals");
    sum.register(&mut plan).unwrap();
    assert_eq!(
        plan.loggable_channels().collect::<Vec<_>>(),
        ["totals", "wire"]
    );
    let captures = GraphCapturePlan::declare(
        &mut plan,
        &CaptureOptions::new(4)
            .exclude("totals")
            .exclude("unmatched"),
    )
    .unwrap();
    assert_eq!(captures.channels(), ["wire"]);
    assert_eq!(
        plan.loggable_channels().collect::<Vec<_>>(),
        ["totals", "wire"]
    );
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut captures = captures.bind(&bindings).unwrap();
    let mut graph = storage.build_with(&bindings).unwrap();
    graph.configure_schedules(|_, schedule| {
        *schedule = CallbackSchedule::on_start()
            .with_execution_duration(std::time::Duration::from_nanos(1));
    });
    let mut graph = testing::UnitTestExecutor::new_with_graph(
        graph,
        testing::UnitTestExecutorConfig {
            start_time: FrameworkTime::from_nanoseconds(7),
            virtual_pool_threads: vec![2],
            ..Default::default()
        },
    );
    graph.run_until_idle(16).unwrap();
    let messages = captures[0].drain_to_vec().unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|(_, bytes)| Value::deserialize(bytes).unwrap().0)
            .collect::<Vec<_>>(),
        [11, 10, 21, 20]
    );
    assert_eq!(
        messages
            .iter()
            .map(|(h, _)| (h.publisher_index, h.batch_index))
            .collect::<Vec<_>>(),
        [(0, 0), (0, 1), (1, 0), (1, 1)]
    );
    assert!(
        messages
            .iter()
            .all(|(h, _)| h.published_at == FrameworkTime::from_nanoseconds(7))
    );
}

#[test]
fn capture_subscribers_do_not_satisfy_task_wiring_requirements_and_can_rebind() {
    let mut plan = GraphPlan::default();
    let publisher = plan.publisher::<Value>("manual", 1).unwrap();
    plan.register_loggable_native::<Value>("manual").unwrap();
    plan.register_loggable_native::<Value>("manual").unwrap();
    assert!(plan.register_loggable_native::<Value>("unknown").is_err());
    plan.publisher::<Hidden>("opaque", 1).unwrap();
    assert!(plan.register_loggable_native::<Value>("opaque").is_err());
    assert!(GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(0)).is_err());
    let first = GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
    let second = GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
    assert!(plan.require("manual", false).is_err());
    assert!(plan.require("manual", true).is_ok());
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut a = first.bind(&bindings).unwrap();
    let mut b = second.bind(&bindings).unwrap();
    let mut publisher = bindings
        .native::<Value>("manual")
        .unwrap()
        .take_publisher(&publisher)
        .unwrap();
    publisher.publish(Value(42)).unwrap();
    publisher.flush(FrameworkTime::from_nanoseconds(0));
    assert_eq!(a[0].drain_to_vec().unwrap(), b[0].drain_to_vec().unwrap());
    drop((a, b, publisher, bindings));
    assert!(storage.bind().is_ok());
}

#[test]
fn nonloggable_only_graph_has_no_automatic_captures() {
    struct Task;
    #[task_callback]
    impl Task {
        fn run(&self, output: &mut Publisher<Hidden>) -> Result<(), LoanError> {
            output.publish(Hidden)
        }
    }
    let mut plan = GraphPlan::default();
    TaskRegistration::new("hidden", Task, CallbackSchedule::input_triggered())
        .register(&mut plan)
        .unwrap();
    let captures = GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
    assert!(captures.channels().is_empty());
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    assert!(captures.bind(&bindings).unwrap().is_empty());
    let mut graph = storage.build_with(&bindings).unwrap();
    graph.configure_schedules(|_, schedule| {
        *schedule = CallbackSchedule::on_start().with_execution_duration(std::time::Duration::ZERO);
    });
    testing::UnitTestExecutor::new_with_graph(graph, Default::default())
        .run_until_idle(8)
        .unwrap();
}

#[test]
fn automatic_replay_preserves_headers_fanout_and_task_publisher_accounting() {
    let mut plan = GraphPlan::default();
    for name in ["first", "second"] {
        let mut task = TaskRegistration::new(name, Sum, CallbackSchedule::input_triggered());
        task.input_channel("input", "wire")
            .output_channel("summary", format!("{name}_result"));
        task.register(&mut plan).unwrap();
    }
    let replay = GraphReplayPlan::declare(
        &mut plan,
        ["wire", "wire", "unknown"],
        &ReplayOptions::new(1).exclude("unknown"),
    )
    .unwrap();
    assert_eq!(replay.channels(), ["wire"]);
    assert!(plan.require("wire", true).is_err());
    let captures = GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(4)).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut sources = replay.bind(&bindings).unwrap();
    let mut captures = captures.bind(&bindings).unwrap();
    let mut graph = storage.build_with(&bindings).unwrap();
    graph.configure_schedules(|_, schedule| {
        *schedule = CallbackSchedule::on_start()
            .with_execution_duration(std::time::Duration::from_nanos(1));
    });
    let mut graph = testing::UnitTestExecutor::new_with_graph(
        graph,
        testing::UnitTestExecutorConfig {
            start_time: FrameworkTime::from_nanoseconds(10),
            virtual_pool_threads: vec![2],
            ..Default::default()
        },
    );
    let header = task::message::MessageHeader {
        published_at: FrameworkTime::from_nanoseconds(9),
        publisher_index: 12,
        batch_index: 3,
    };
    sources[0].inject(header, &10_u64.to_le_bytes()).unwrap();
    sources[0]
        .inject(
            task::message::MessageHeader {
                batch_index: 4,
                ..header
            },
            &20_u64.to_le_bytes(),
        )
        .unwrap();
    assert!(sources[0].inject(header, b"bad").is_err());
    graph.run_until_idle(8).unwrap();
    for capture in &mut captures {
        let messages = capture.drain_to_vec().unwrap();
        if capture.channel() == "wire" {
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0].0, header);
            assert_eq!(messages[1].0.batch_index, 4);
        } else {
            assert_eq!(Value::deserialize(&messages[0].1).unwrap().0, 30);
        }
    }
}

#[test]
fn replay_planning_rejects_missing_and_contextual_decoders_without_mutating_publishers() {
    struct Consumer;
    #[task_callback]
    impl Consumer {
        fn run(
            &self,
            #[trigger(true)]
            #[capacity(4)]
            input: InputSpan<Value>,
            #[trigger(true)]
            #[capacity(4)]
            opaque: InputSpan<Hidden>,
            #[trigger(true)]
            #[capacity(4)]
            contextual: InputSpan<Contextual>,
        ) {
            let _ = (input, opaque, contextual);
        }
    }
    let mut plan = GraphPlan::default();
    TaskRegistration::new("consumer", Consumer, CallbackSchedule::input_triggered())
        .register(&mut plan)
        .unwrap();
    assert_eq!(plan.replayable_channels().collect::<Vec<_>>(), ["input"]);
    assert_eq!(
        plan.loggable_channels().collect::<Vec<_>>(),
        ["contextual", "input"]
    );
    for channel in [
        "opaque",
        "contextual",
        "missing",
        task::recording::EXECUTION_LOG_CHANNEL,
    ] {
        assert!(
            GraphReplayPlan::declare(&mut plan, ["input", channel], &ReplayOptions::new(1))
                .is_err()
        );
    }
    assert!(GraphReplayPlan::declare(&mut plan, ["input"], &ReplayOptions::new(0)).is_err());
    let key = plan.native::<Value>("input").unwrap().publisher(1);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    assert_eq!(
        bindings
            .native::<Value>("input")
            .unwrap()
            .take_publisher(&key)
            .unwrap()
            .publisher_index(),
        0
    );
}

#[cfg(feature = "serde")]
#[test]
fn log_driven_source_selection_carries_exclusions_into_replay_feed() {
    use logging::{OwnedLogEntry, SortedLogStreamReader};
    use std::collections::HashMap;
    let mut plan = GraphPlan::default();
    TaskRegistration::new("sum", Sum, CallbackSchedule::input_triggered())
        .register(&mut plan)
        .unwrap();
    let header = task::message::MessageHeader {
        published_at: FrameworkTime::from_nanoseconds(5),
        publisher_index: 2,
        batch_index: 1,
    };
    let reader = SortedLogStreamReader::from_entries(
        vec![
            OwnedLogEntry {
                channel_name: "input".into(),
                header,
                serialized_body: 42_u64.to_le_bytes().to_vec(),
            },
            OwnedLogEntry {
                channel_name: "excluded".into(),
                header,
                serialized_body: vec![],
            },
        ],
        HashMap::new(),
    )
    .unwrap();
    let replay = GraphReplayPlan::from_log(
        &mut plan,
        &reader,
        &ReplayOptions::new(1).exclude("excluded"),
    )
    .unwrap();
    assert_eq!(replay.channels(), ["input"]);
    let captures =
        GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(1).exclude("summary")).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut captures = captures.bind(&bindings).unwrap();
    let mut feed = replay.bind_feed(&bindings, reader).unwrap();
    feed.inject_due_while(header.published_at, |_, _, _| Ok(()), || true)
        .unwrap();
    assert!(feed.exhausted());
    assert_eq!(
        captures[0].drain_to_vec().unwrap(),
        [(header, 42_u64.to_le_bytes().to_vec())]
    );
}

#[cfg(feature = "serde")]
#[test]
fn log_driven_planning_rejects_incomplete_logs_before_allocating_sources() {
    let mut plan = GraphPlan::default();
    plan.native::<Value>("data").unwrap();
    plan.register_replay_native::<Value>("data").unwrap();
    let reader = logging::SortedLogStreamReader::from_entries(
        vec![],
        std::collections::HashMap::from([(
            logging::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT.into(),
            b"null".to_vec(),
        )]),
    )
    .unwrap();
    assert!(
        matches!(GraphReplayPlan::from_log(&mut plan, &reader, &ReplayOptions::new(1)), Err(e) if e.to_string().contains("incomplete"))
    );
}

#[cfg(feature = "serde")]
#[test]
fn serde_payloads_are_discovered_without_explicit_codec_registration() {
    struct SerdeSource;
    #[task_callback]
    impl SerdeSource {
        fn run(&self, output: &mut Publisher<u64>) -> Result<(), LoanError> {
            output.publish(42)
        }
    }
    let mut plan = GraphPlan::default();
    TaskRegistration::new("serde", SerdeSource, CallbackSchedule::input_triggered())
        .register(&mut plan)
        .unwrap();
    let captures = GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
    assert_eq!(captures.channels(), ["output"]);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut captures = captures.bind(&bindings).unwrap();
    let mut graph = storage.build_with(&bindings).unwrap();
    graph.configure_schedules(|_, schedule| {
        *schedule = CallbackSchedule::on_start().with_execution_duration(std::time::Duration::ZERO);
    });
    testing::UnitTestExecutor::new_with_graph(graph, Default::default())
        .run_until_idle(8)
        .unwrap();
    assert_eq!(captures[0].drain_to_vec().unwrap()[0].1, b"42");
}

#[cfg(feature = "serde")]
#[test]
fn automatic_captures_populate_session_descriptor_and_persist_overflow() {
    use logging::{ExecutionRecorder, LogFileReader, LogSession};
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    #[derive(Clone, Default)]
    struct Bytes(Arc<Mutex<Vec<u8>>>);
    impl Write for Bytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for capacity in [1, 2] {
        let mut plan = GraphPlan::default();
        TaskRegistration::new("source", Source(5), CallbackSchedule::input_triggered())
            .register(&mut plan)
            .unwrap();
        let captures =
            GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(capacity)).unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let recorder = ExecutionRecorder::new(1);
        let mut graph = storage
            .build_with(&bindings)
            .unwrap()
            .with_recording(&recorder)
            .unwrap();
        graph.configure_schedules(|_, schedule| {
            *schedule =
                CallbackSchedule::on_start().with_execution_duration(std::time::Duration::ZERO);
        });
        let mut graph = testing::UnitTestExecutor::new_with_graph(graph, Default::default());
        let bytes = Bytes::default();
        let session = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            captures.bind(&bindings).unwrap(),
        )
        .with_recording(recorder)
        .unwrap();
        graph.run_until_idle(8).unwrap();
        assert_eq!(session.finish().is_ok(), capacity == 2);
        let reader = logging::log_file_json::JsonLogFileReader::from_reader(
            bytes.0.lock().unwrap().as_slice(),
        )
        .unwrap();
        let descriptor: task::recording::ExecutionDescriptor = serde_json::from_slice(
            reader
                .artifact(task::recording::EXECUTION_LOG_DESCRIPTOR_ARTIFACT)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(descriptor.logged_channels, ["output"]);
        assert_eq!(
            reader
                .artifact(logging::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT)
                .is_some(),
            capacity == 1
        );
    }
}
