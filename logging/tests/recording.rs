#![cfg(feature = "serde")]
use logging::{
    CapturePlan, ExecutionRecorder, LogFileReader, LogFileWriter, LogSession, ReplaySourcePlan,
};
use std::{
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use task::{
    CallbackSchedule, ChannelPlan, GraphBuilder, GraphPlan, LoanError, Publisher, RequiredInput,
    message::MessageHeader,
    recording::{
        EXECUTION_LOG_CHANNEL, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, ExecutionDescriptor,
        ExecutionRecord, Outcome,
    },
    time::FrameworkTime,
};
use task_macros::task_callback;

fn at(n: i64) -> FrameworkTime {
    FrameworkTime::from_nanoseconds(n)
}
fn header(n: i64) -> MessageHeader {
    MessageHeader::new(at(n))
}

#[test]
fn recording_modes_preserve_execution_and_select_metadata() {
    use task::recording::{RecordingMode, RecordingOptions};
    for default in [
        RecordingMode::Off,
        RecordingMode::DurationOnly,
        RecordingMode::Full,
    ] {
        let mut plan = task::automatic::NamedPlan::default();
        let tasks: Vec<_> = ["default", "off", "duration", "full"]
            .into_iter()
            .map(|name| {
                task::automatic::TaskRegistration::new(
                    name,
                    Source { fail: 0 },
                    CallbackSchedule::default(),
                )
                .register(&mut plan)
                .unwrap()
            })
            .collect();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut graph = storage.graph_builder();
        for task in tasks {
            task.add_to_graph(&mut graph, &bindings);
        }
        let recorder = ExecutionRecorder::new(4);
        let options = RecordingOptions::new(default)
            .with_callback("off", RecordingMode::Off)
            .with_callback("duration", RecordingMode::DurationOnly)
            .with_callback("full", RecordingMode::Full);
        let mut graph = recorder
            .attach_with_options(graph.build().unwrap(), options)
            .unwrap();
        graph.step(at(7)).unwrap();
        let records = recorder.drain();
        assert_eq!(
            records.len(),
            if default == RecordingMode::Off { 2 } else { 3 }
        );
        let descriptor = recorder.descriptor().unwrap();
        for record in records {
            assert_eq!(record.execution_time, at(7));
            assert_eq!(record.outcome, Outcome::Committed);
            assert_eq!(
                record.outputs.len(),
                usize::from(
                    descriptor.callbacks[record.callback_id.index()].recording_mode
                        == RecordingMode::Full
                )
            );
        }
        assert_eq!(descriptor.callbacks[0].recording_mode, default);
        assert_eq!(descriptor.callbacks[1].recording_mode, RecordingMode::Off);
    }
}

#[test]
fn reduced_recording_does_not_visit_payloads_and_retains_failure_outcomes() {
    use task::recording::{RecordingMode, RecordingOptions};
    struct Bare;
    impl task::Callback for Bare {
        fn run(&mut self, _: &task::Context) -> Result<(), LoanError> {
            Err(LoanError::LoanCapacityReached)
        }
        fn visit_prepared_messages(&self, _: &mut dyn FnMut(task::recording::LoggedMessage)) {
            panic!("payload visitation disabled");
        }
    }
    for mode in [RecordingMode::Off, RecordingMode::DurationOnly] {
        let mut graph = GraphBuilder::new();
        graph.add_callback("bare", || Ok(Bare));
        let recorder = ExecutionRecorder::new(1);
        let mut graph = recorder
            .attach_with_options(graph.build().unwrap(), RecordingOptions::new(mode))
            .unwrap();
        assert!(graph.step(at(1)).is_err());
        let records = recorder.drain();
        assert_eq!(
            records.len(),
            usize::from(mode == RecordingMode::DurationOnly)
        );
        if let Some(record) = records.first() {
            assert!(matches!(record.outcome, Outcome::BodyError(_)));
        }
    }
}

#[test]
fn diagnostic_policies_keep_channel_time_header_and_flush_before_panic() {
    use logging::{DiagnosticKind, DiagnosticPolicy};
    for policy in [
        DiagnosticPolicy::Silent,
        DiagnosticPolicy::Print,
        DiagnosticPolicy::Panic,
    ] {
        let mut plan = ChannelPlan::<u64>::new("diagnostic-output");
        let source = ReplaySourcePlan::declare(&mut plan, 1);
        let capture = CapturePlan::declare(&mut plan, 1);
        let storage = GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build();
        source
            .bind(&bindings)
            .unwrap()
            .inject(header(5), b"42")
            .unwrap();
        let flushes = Arc::new(AtomicUsize::new(0));
        let mut log = LogSession::new(
            FailingWriter {
                flushes: flushes.clone(),
                panic: false,
            },
            vec![capture.bind(&bindings).unwrap()],
        )
        .with_diagnostic_policy(policy);
        let status = log.status();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log.flush_at(at(9))));
        assert_eq!(result.is_err(), policy == DiagnosticPolicy::Panic);
        if let Ok(result) = result {
            assert!(result.is_err());
        }
        assert_eq!(flushes.load(Ordering::SeqCst), 2);
        let diagnostics = status.diagnostics();
        assert_eq!(diagnostics[0].kind, DiagnosticKind::Write);
        assert_eq!(
            status
                .intern_tables()
                .channels
                .lookup_by_id(diagnostics[0].channel.unwrap()),
            "diagnostic-output"
        );
        assert_eq!(diagnostics[0].at, Some(at(9)));
        assert_eq!(diagnostics[0].header, Some(header(5)));
        drop(log);
    }
}
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
struct Double;
#[task_callback]
impl Double {
    fn run(&self, input: RequiredInput<u64>, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(*input * 2)
    }
}
#[test]
fn planned_capture_replay_and_execution_records_round_trip_through_json() {
    let mut input = ChannelPlan::new("input");
    let mut output = ChannelPlan::new("output");
    let task = Double::declare(&mut input, &mut output).unwrap();
    let injection = ReplaySourcePlan::declare(&mut input, 1);
    let input_log = CapturePlan::declare(&mut input, 2);
    let output_log = CapturePlan::declare(&mut output, 2);
    let storage = GraphPlan::new((input, output)).allocate().unwrap();
    let input = storage.channels().0.build();
    let output = storage.channels().1.build();
    let bytes = Bytes::default();
    let recorder = ExecutionRecorder::new(4);
    let status;
    {
        let mut graph = GraphBuilder::with_storage(&storage);
        graph.add_scheduled_callback(
            "double",
            CallbackSchedule::default().with_execution_duration(Duration::from_nanos(3)),
            || Ok(Double.bind(task, &input, &output)?),
        );
        let graph = recorder.attach(graph.build().unwrap()).unwrap();
        let log = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            vec![
                input_log.bind(&input).unwrap(),
                output_log.bind(&output).unwrap(),
            ],
        )
        .with_recording(recorder.clone())
        .unwrap();
        status = log.status();
        let mut simulation = simulation_executor::SimulationState::with_config(
            graph,
            simulation_executor::SimulationConfig {
                start_time: at(10),
                ..Default::default()
            },
        )
        .unwrap();
        let mut injection = injection.bind(&input).unwrap();
        injection.inject(header(5), b"21").unwrap();
        assert_eq!(simulation.step().unwrap().executed, [0]);
        assert_eq!(simulation.current_time(), at(13));
    }
    assert!(status.errors().is_empty());
    let raw = bytes.0.lock().unwrap().clone();
    let reader = logging::log_file_json::JsonLogFileReader::from_reader(raw.as_slice()).unwrap();
    let descriptor: ExecutionDescriptor =
        serde_json::from_slice(reader.artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT).unwrap())
            .unwrap();
    assert_eq!(descriptor.logged_channels, ["input", "output"]);
    assert_eq!(descriptor.callbacks[0].name, "double");
    assert_eq!(descriptor.callbacks[0].endpoints[0].ordinal, 0);
    assert_eq!(descriptor.callbacks[0].endpoints[1].ordinal, 0);
    let tables = reader.intern_tables().unwrap();
    let first: serde_json::Value =
        serde_json::from_slice(raw.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    assert_eq!(first["artifact"], logging::INTERN_TABLES_ARTIFACT);
    assert_eq!(reader.len(), 3);
    assert_eq!(reader.entry(0).unwrap().header, header(5));
    assert_eq!(reader.entry(1).unwrap().serialized_body, b"42");
    let entry = reader.entry(2).unwrap();
    assert_eq!(entry.channel_name, EXECUTION_LOG_CHANNEL);
    let record: ExecutionRecord = serde_json::from_slice(entry.serialized_body).unwrap();
    assert_eq!(tables.callbacks.lookup_by_id(record.callback_id), "double");
    assert!(
        serde_json::from_slice::<serde_json::Value>(entry.serialized_body).unwrap()["callback_id"]
            .is_number()
    );
    assert_eq!(record.execution_time, at(10));
    assert_eq!(record.inputs[0].header, header(5));
    assert_eq!(record.outputs[0].header, header(10));
    assert_eq!(record.outcome, Outcome::Committed);
}

#[test]
fn startup_tables_are_shared_by_shards_and_resolve_diagnostics_after_graph_drop() {
    use task::automatic::{NamedPlan, TaskRegistration};
    let bytes = Bytes::default();
    let status;
    {
        let mut plan = NamedPlan::default();
        let mut tasks = Vec::new();
        for name in ["z", "a"] {
            let mut task =
                TaskRegistration::new(name, Source { fail: 0 }, CallbackSchedule::default());
            task.output_channel("output", format!("{name}-output"));
            tasks.push(task.register(&mut plan).unwrap());
        }
        let capture =
            logging::AutomaticCapturePlan::declare(&mut plan, &logging::CaptureOptions::new(4))
                .unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut graph = storage.graph_builder();
        for task in tasks {
            task.add_to_graph(&mut graph, &bindings);
        }
        let recorder = ExecutionRecorder::new(8);
        let graph = recorder.attach(graph.build().unwrap()).unwrap();
        let logger = logging::LoggingScope::new(
            RecoveringWriter {
                inner: logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
                fail_message: true,
                fail_artifact: false,
                fail_flush: false,
            },
            capture.bind(&bindings).unwrap(),
            2,
        )
        .with_recording(recorder)
        .unwrap();
        status = logger.status();
        {
            let mut graph = logger
                .attach_periodic(graph, Duration::from_nanos(1))
                .unwrap();
            let raw = bytes.0.lock().unwrap().clone();
            let lines: Vec<serde_json::Value> = raw
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).unwrap())
                .collect();
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[0]["artifact"], logging::INTERN_TABLES_ARTIFACT);
            assert_eq!(lines[1]["artifact"], EXECUTION_LOG_DESCRIPTOR_ARTIFACT);
            let tables = status.intern_tables();
            for name in ["z", "a", "LogTask[0]", "LogTask[1]"] {
                assert_eq!(
                    tables.callbacks.lookup_by_value(name),
                    graph.metadata().callback_names.lookup_by_value(name)
                );
            }
            for name in ["z-output", "a-output"] {
                assert_eq!(
                    tables.channels.lookup_by_value(name),
                    graph.metadata().channel_names.lookup_by_value(name)
                );
            }
            graph.step(at(0)).unwrap();
        }
        assert!(logger.finish().is_err());
    }
    let raw = bytes.0.lock().unwrap().clone();
    let lines: Vec<serde_json::Value> = raw
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(
        lines
            .iter()
            .filter(|line| line["artifact"] == logging::INTERN_TABLES_ARTIFACT)
            .count(),
        1
    );
    let reader = logging::log_file_json::JsonLogFileReader::from_reader(raw.as_slice()).unwrap();
    let tables = reader.intern_tables().unwrap();
    let incomplete: logging::incompleteness::RecordingIncompleteness = serde_json::from_slice(
        reader
            .artifact(logging::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT)
            .unwrap(),
    )
    .unwrap();
    let diagnostic = &incomplete.diagnostics[0];
    let channel = tables.channels.lookup_by_id(diagnostic.channel.unwrap());
    assert!(matches!(channel, "a-output" | "z-output"));
    let status_tables = status.intern_tables();
    assert_eq!(
        status_tables
            .channels
            .lookup_by_id(diagnostic.channel.unwrap()),
        channel
    );
    assert!(status.errors().iter().any(|error| error.contains(channel)));
    for entry in reader
        .iter()
        .filter(|entry| entry.channel_name == EXECUTION_LOG_CHANNEL)
    {
        let record: ExecutionRecord = serde_json::from_slice(entry.serialized_body).unwrap();
        assert!(matches!(
            tables.callbacks.lookup_by_id(record.callback_id),
            "a" | "z"
        ));
    }
}

struct Source {
    fail: u8,
}
#[task_callback]
impl Source {
    fn run(&self, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(42)?;
        assert_ne!(self.fail, 2, "body panic");
        if self.fail == 1 {
            return Err(LoanError::LoanCapacityReached);
        }
        Ok(())
    }
}
#[test]
fn failed_batches_and_timing_rejection_do_not_record_successful_publications() {
    for fail in [1, 2, 3] {
        let mut plan = ChannelPlan::new("output");
        let a = Source::declare(&mut plan).unwrap();
        let b = Source::declare(&mut plan).unwrap();
        let capture = CapturePlan::declare(&mut plan, 2);
        let storage = GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build();
        let mut graph = GraphBuilder::new();
        graph.add_scheduled_callback(
            "a",
            CallbackSchedule::on_start().with_execution_duration(Duration::from_nanos(1)),
            || Ok(Source { fail: 0 }.bind(a, &bindings)?),
        );
        graph.add_scheduled_callback(
            "b",
            CallbackSchedule::on_start().with_execution_duration(if fail == 3 {
                Duration::MAX
            } else {
                Duration::from_nanos(1)
            }),
            || Ok(Source { fail }.bind(b, &bindings)?),
        );
        let recorder = ExecutionRecorder::new(4);
        let graph = recorder.attach(graph.build().unwrap()).unwrap();
        let mut sim = simulation_executor::SimulationState::with_config(
            graph,
            simulation_executor::SimulationConfig {
                virtual_pool_threads: vec![2],
                node_executor_thread_count: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(sim.step().is_err());
        let records = recorder.drain();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.outputs.is_empty()));
        assert!(records.iter().any(|r| r.outcome == Outcome::Cancelled));
        assert!(records.iter().any(|r| match fail {
            1 => matches!(r.outcome, Outcome::BodyError(_)),
            2 => r.outcome == Outcome::BodyPanicked,
            _ => r.outcome == Outcome::Cancelled,
        }));
        assert!(
            capture
                .bind(&bindings)
                .unwrap()
                .drain_to_vec()
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn commit_panic_is_marked_indeterminate_and_payload_capture_retains_prefix() {
    struct Broken<'a>(Publisher<'a, u64>);
    impl task::Callback for Broken<'_> {
        fn recording_endpoints(&self) -> Option<Vec<task::recording::EndpointDescriptor>> {
            Some(vec![])
        }
        fn run(&mut self, _: &task::Context) -> Result<(), LoanError> {
            self.0.publish(9)
        }
        fn flush_outputs(&mut self, time: FrameworkTime) {
            self.0.flush(time);
            panic!("after publish");
        }
        fn discard_outputs(&mut self) {
            self.0.discard_pending();
        }
    }
    let mut plan = ChannelPlan::new("output");
    let key = plan.publisher(1);
    let capture = CapturePlan::declare(&mut plan, 1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut graph = GraphBuilder::new();
    graph.add_callback("broken", || Ok(Broken(bindings.take_publisher(&key)?)));
    let recorder = ExecutionRecorder::new(1);
    let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| graph.step(at(0)))).is_err());
    assert_eq!(recorder.drain()[0].outcome, Outcome::CommitPanicked);
    assert_eq!(
        capture.bind(&bindings).unwrap().drain_to_vec().unwrap()[0].1,
        b"9"
    );
}

#[test]
fn bounded_recording_and_capture_overflow_are_observable() {
    let mut plan = ChannelPlan::new("output");
    let declaration = Source::declare(&mut plan).unwrap();
    let capture = CapturePlan::declare(&mut plan, 1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut graph = GraphBuilder::new();
    graph.add_callback("source", || {
        Ok(Source { fail: 0 }.bind(declaration, &bindings)?)
    });
    let recorder = ExecutionRecorder::new(1);
    let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
    graph.step(at(0)).unwrap();
    graph.step(at(1)).unwrap();
    assert_eq!(recorder.dropped(), 1);
    assert!(
        capture
            .bind(&bindings)
            .unwrap()
            .drain_to_vec()
            .unwrap_err()
            .to_string()
            .contains("overflow")
    );
    let log = LogSession::new(
        logging::log_file_json::JsonLogFileWriter::new(Bytes::default()),
        vec![],
    )
    .with_recording(recorder)
    .unwrap();
    assert!(log.finish().unwrap_err().to_string().contains("overflow"));
}

#[test]
fn incompleteness_persists_all_overflows_and_is_sticky_across_flushes() {
    use logging::incompleteness::{RECORDING_INCOMPLETENESS_ARTIFACT, RecordingIncompleteness};
    for (capture_capacity, recorder_capacity) in [(1, 4), (4, 1), (1, 1)] {
        let mut plan = ChannelPlan::new("output");
        let declaration = Source::declare(&mut plan).unwrap();
        let capture = CapturePlan::declare(&mut plan, capture_capacity);
        let storage = GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build();
        let mut graph = GraphBuilder::new();
        graph.add_callback("source", || {
            Ok(Source { fail: 0 }.bind(declaration, &bindings)?)
        });
        let recorder = ExecutionRecorder::new(recorder_capacity);
        let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
        let bytes = Bytes::default();
        let mut session = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            vec![capture.bind(&bindings).unwrap()],
        )
        .with_recording(recorder)
        .unwrap();
        session.flush().unwrap();
        assert!(
            logging::log_file_json::JsonLogFileReader::from_reader(
                bytes.0.lock().unwrap().as_slice()
            )
            .unwrap()
            .artifact(RECORDING_INCOMPLETENESS_ARTIFACT)
            .is_none()
        );
        graph.step(at(0)).unwrap();
        graph.step(at(1)).unwrap();
        assert!(session.flush().is_err());
        graph.step(at(2)).unwrap();
        assert!(session.finish().is_err());
        let raw = bytes.0.lock().unwrap().clone();
        let reader =
            logging::log_file_json::JsonLogFileReader::from_reader(raw.as_slice()).unwrap();
        let snapshot: RecordingIncompleteness =
            serde_json::from_slice(reader.artifact(RECORDING_INCOMPLETENESS_ARTIFACT).unwrap())
                .unwrap();
        assert_eq!(
            snapshot.recorder_entries_dropped,
            usize::from(recorder_capacity == 1)
        );
        assert_eq!(
            snapshot.captures[0].writer_drops + snapshot.captures[0].reader_drops,
            u64::from(capture_capacity == 1)
        );
        if capture_capacity == 1 && recorder_capacity == 1 {
            assert!(snapshot.errors.iter().any(|e| e.contains("capture")));
            assert!(
                snapshot
                    .errors
                    .iter()
                    .any(|e| e.contains("execution recording overflow"))
            );
        }
        #[cfg(not(miri))]
        let sorted = logging::SortedLogStreamReader::from_reader(raw.as_slice(), 2).unwrap();
        #[cfg(miri)]
        let sorted = logging::SortedLogStreamReader::from_entries(
            vec![],
            std::collections::HashMap::from([(
                RECORDING_INCOMPLETENESS_ARTIFACT.into(),
                reader
                    .artifact(RECORDING_INCOMPLETENESS_ARTIFACT)
                    .unwrap()
                    .to_vec(),
            )]),
        )
        .unwrap();
        assert!(
            matches!(logging::ReplayFeed::new(sorted, [], Default::default()), Err(e) if e.to_string().contains("incomplete"))
        );
    }
}

struct RecoveringWriter {
    inner: logging::log_file_json::JsonLogFileWriter<Bytes>,
    fail_message: bool,
    fail_artifact: bool,
    fail_flush: bool,
}
impl LogFileWriter for RecoveringWriter {
    fn store_message(
        &mut self,
        channel: &str,
        header: &MessageHeader,
        body: &[u8],
    ) -> Result<(), logging::BoxedLogError> {
        if std::mem::take(&mut self.fail_message) {
            return Err("message sink failure".into());
        }
        self.inner.store_message(channel, header, body)
    }
    fn write_artifact(&mut self, name: &str, body: &[u8]) -> Result<(), logging::BoxedLogError> {
        if std::mem::take(&mut self.fail_artifact) {
            return Err("artifact sink failure".into());
        }
        self.inner.write_artifact(name, body)
    }
    fn flush(&mut self) -> Result<(), logging::BoxedLogError> {
        if std::mem::take(&mut self.fail_flush) {
            return Err("flush sink failure".into());
        }
        self.inner.flush()
    }
}

#[test]
fn recovered_writer_cannot_turn_failed_recording_into_a_complete_log() {
    use logging::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT;
    for (fail_message, fail_artifact, fail_flush) in [
        (true, false, false),
        (true, true, false),
        (false, false, true),
    ] {
        let mut plan = ChannelPlan::<u64>::new("output");
        let source = ReplaySourcePlan::declare(&mut plan, 1);
        let capture = CapturePlan::declare(&mut plan, 1);
        let storage = GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build();
        source
            .bind(&bindings)
            .unwrap()
            .inject(header(0), b"42")
            .unwrap();
        let bytes = Bytes::default();
        let mut session = LogSession::new(
            RecoveringWriter {
                inner: logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
                fail_message,
                fail_artifact,
                fail_flush,
            },
            vec![capture.bind(&bindings).unwrap()],
        );
        assert!(session.flush().is_err());
        if fail_artifact && fail_message {
            assert!(session.flush().is_err());
        }
        session.flush().unwrap();
        assert!(session.finish().is_err());
        let reader = logging::log_file_json::JsonLogFileReader::from_reader(
            bytes.0.lock().unwrap().as_slice(),
        )
        .unwrap();
        assert!(reader.artifact(RECORDING_INCOMPLETENESS_ARTIFACT).is_some());
    }
}

#[test]
fn failed_descriptor_write_attempts_a_marker_during_cleanup() {
    let mut plan = ChannelPlan::new("output");
    let declaration = Source::declare(&mut plan).unwrap();
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut graph = GraphBuilder::new();
    graph.add_callback("source", || {
        Ok(Source { fail: 0 }.bind(declaration, &bindings)?)
    });
    let recorder = ExecutionRecorder::new(1);
    let _graph = recorder.attach(graph.build().unwrap()).unwrap();
    let bytes = Bytes::default();
    let session = LogSession::new(
        RecoveringWriter {
            inner: logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            fail_message: false,
            fail_artifact: true,
            fail_flush: false,
        },
        vec![],
    );
    let status = session.status();
    assert!(session.with_recording(recorder).is_err());
    assert!(status.errors()[0].contains("startup artifacts"));
    let reader =
        logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap();
    assert!(
        reader
            .artifact(logging::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT)
            .is_some()
    );
}

#[test]
fn serialization_failure_is_marked_on_session_drop() {
    struct Unserializable;
    impl task::loggable::Loggable for Unserializable {
        type Context<'a> = ();
        fn deserialize_with_ctx<'a>(
            _: &[u8],
            _: (),
        ) -> Result<Self, task::loggable::DeserializeError>
        where
            Self: 'a,
        {
            Ok(Self)
        }
        fn serialize(&self, _: &mut dyn Write) -> Result<(), task::loggable::SerializeError> {
            Err("payload serialization failed".into())
        }
    }
    let mut plan = ChannelPlan::new("broken");
    let publisher = plan.publisher(1);
    let capture = CapturePlan::declare(&mut plan, 1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut publisher = bindings.take_publisher(&publisher).unwrap();
    publisher.publish(Unserializable).unwrap();
    publisher.flush(at(0));
    let bytes = Bytes::default();
    let session = LogSession::new(
        logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
        vec![capture.bind(&bindings).unwrap()],
    );
    let status = session.status();
    drop(session);
    assert!(status.errors()[0].contains("serialization failed"));
    let reader =
        logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap();
    assert!(
        reader
            .artifact(logging::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT)
            .is_some()
    );
}

#[test]
fn metadata_panic_before_commit_cannot_leave_outputs_on_a_cancelled_record() {
    struct MetadataFailure;
    impl task::Callback for MetadataFailure {
        fn recording_endpoints(&self) -> Option<Vec<task::recording::EndpointDescriptor>> {
            Some(vec![])
        }
        fn run(&mut self, _: &task::Context) -> Result<(), LoanError> {
            Ok(())
        }
        fn visit_pending_messages(&self, visit: &mut dyn FnMut(task::recording::LoggedMessage)) {
            visit(task::recording::LoggedMessage {
                ordinal: 0,
                header: header(0),
            });
            panic!("metadata failed before output flush");
        }
    }
    let mut graph = GraphBuilder::new();
    graph.add_callback("metadata_failure", || Ok(MetadataFailure));
    let recorder = ExecutionRecorder::new(1);
    let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| graph.step(at(0)))).is_err());
    let records = recorder.drain();
    assert_eq!(records[0].outcome, Outcome::Cancelled);
    assert!(records[0].outputs.is_empty());
}

struct FailingWriter {
    flushes: Arc<AtomicUsize>,
    panic: bool,
}
impl LogFileWriter for FailingWriter {
    fn store_message(
        &mut self,
        _: &str,
        _: &MessageHeader,
        _: &[u8],
    ) -> Result<(), logging::BoxedLogError> {
        Err("write failed".into())
    }
    fn write_artifact(&mut self, _: &str, _: &[u8]) -> Result<(), logging::BoxedLogError> {
        Ok(())
    }
    fn flush(&mut self) -> Result<(), logging::BoxedLogError> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "writer panic");
        Err("flush failed".into())
    }
}
#[test]
fn final_flush_runs_during_assertion_unwind_and_reports_writer_errors() {
    for panic in [false, true] {
        let flushes = Arc::new(AtomicUsize::new(0));
        let session = LogSession::new(
            FailingWriter {
                flushes: flushes.clone(),
                panic,
            },
            vec![],
        );
        let status = session.status();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _session = session;
                panic!("test assertion");
            }))
            .is_err()
        );
        assert_eq!(flushes.load(Ordering::SeqCst), if panic { 1 } else { 2 });
        assert_eq!(status.errors().len(), if panic { 1 } else { 2 });
    }
}

#[test]
fn capture_write_failure_still_attempts_final_writer_flush() {
    let mut plan = ChannelPlan::<u64>::new("output");
    let replay = ReplaySourcePlan::declare(&mut plan, 1);
    let capture = CapturePlan::declare(&mut plan, 1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    replay
        .bind(&bindings)
        .unwrap()
        .inject(header(0), b"42")
        .unwrap();
    let flushes = Arc::new(AtomicUsize::new(0));
    let log = LogSession::new(
        FailingWriter {
            flushes: flushes.clone(),
            panic: false,
        },
        vec![capture.bind(&bindings).unwrap()],
    );
    let status = log.status();
    assert!(
        log.finish()
            .unwrap_err()
            .to_string()
            .contains("write failed")
    );
    assert_eq!(flushes.load(Ordering::SeqCst), 2);
    assert_eq!(status.errors().len(), 3);
}

#[test]
fn scoped_live_execution_uses_the_same_recording_lifecycle() {
    let mut plan = ChannelPlan::new("output");
    let declaration = Source::declare(&mut plan).unwrap();
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut graph = GraphBuilder::new();
    graph.add_scheduled_callback("source", CallbackSchedule::on_start(), || {
        Ok(Source { fail: 0 }.bind(declaration, &bindings)?)
    });
    let recorder = ExecutionRecorder::new(4);
    let graph = recorder.attach(graph.build().unwrap()).unwrap();
    let executor = live_executor::LiveExecutor::new(1, graph).unwrap();
    executor
        .run_with(|stop| {
            let start = std::time::Instant::now();
            while recorder.drain().is_empty() {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "callback never recorded"
                );
                std::thread::yield_now();
            }
            stop.request_stop();
        })
        .unwrap();
}
