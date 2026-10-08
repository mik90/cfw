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
    assert_eq!(reader.len(), 3);
    assert_eq!(reader.entry(0).unwrap().header, header(5));
    assert_eq!(reader.entry(1).unwrap().serialized_body, b"42");
    let entry = reader.entry(2).unwrap();
    assert_eq!(entry.channel_name, EXECUTION_LOG_CHANNEL);
    let record: ExecutionRecord = serde_json::from_slice(entry.serialized_body).unwrap();
    assert_eq!(record.execution_time, at(10));
    assert_eq!(record.inputs[0].header, header(5));
    assert_eq!(record.outputs[0].header, header(10));
    assert_eq!(record.outcome, Outcome::Committed);
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
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
        assert_eq!(status.errors().len(), 1);
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
    assert_eq!(flushes.load(Ordering::SeqCst), 1);
    assert_eq!(status.errors().len(), 2);
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
