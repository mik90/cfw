use exact_replay_executor::{
    DivergencePolicy, ExactReplayConfig, ExactReplayExecutor, ExactReplayStepError, OutputCapture,
    ReplayBindings, ReplayInputPlan, ReplayLog,
};
use logging::{CapturePlan, ExecutionLogOptions, ExecutionLogPlan, LogSession};
use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex},
};
use task::{ChannelPlan, LoanError, Output, RequiredInput, time::FrameworkTime};
use task_graph::GraphBuilder;
use task_macros::task_callback;

fn at(n: i64) -> FrameworkTime {
    FrameworkTime::from_nanos(n)
}

fn bounded<T: Copy + Default, const N: usize>(
    items: impl IntoIterator<Item = T>,
) -> task::StaticVec<T, N> {
    let mut values = task::StaticVec::new();
    for item in items {
        assert!(values.push(item));
    }
    values
}
#[derive(Default, Clone)]
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
struct Value(u64);
impl task::loggable::Loggable for Value {
    type Context<'a> = ();
    fn serialize(&self, writer: &mut dyn Write) -> Result<(), task::loggable::SerializeError> {
        if self.0 == u64::MAX {
            return Err("deliberate codec error".into());
        }
        serde_json::to_writer(writer, &self.0).map_err(Into::into)
    }
    fn deserialize_with_ctx<'a>(
        bytes: &[u8],
        _: (),
    ) -> Result<Self, task::loggable::DeserializeError>
    where
        Self: 'a,
    {
        serde_json::from_slice(bytes).map(Self).map_err(Into::into)
    }
}
fn source_body<'a>(mode: u8) -> task::testing::FakePublisherBody<'a, Value> {
    let mut next = 1;
    Box::new(move |publisher, context| {
        let mut names = task::string_interner::CallbackNameInterner::new();
        assert_eq!(
            context.callback_names.lookup_by_value("source"),
            Some(names.intern("source"))
        );
        if mode == 3 {
            panic!("callback panic");
        }
        if mode == 5 {
            publisher.set_publisher_index(999);
        }
        if matches!(mode, 6 | 7) {
            let header = task::message::MessageHeader {
                published_at: if mode == 6 {
                    at(context.now().to_nanoseconds() + 1)
                } else {
                    context.now()
                },
                publisher_index: publisher.publisher_index(),
                batch_index: u32::from(mode == 7),
            };
            publisher.publish_with_header(header, Value(next))?;
            next += 1;
            return Ok(());
        }
        if mode != 2 {
            publisher.publish(Value(if mode == 4 { u64::MAX } else { next }))?;
        }
        if mode == 1 {
            publisher.publish(Value(999))?;
        }
        next += 1;
        Ok(())
    })
}

fn source_callback<'a>(
    bindings: &task::ChannelBindings<'a, Value>,
    key: &task::PublisherKey<Value>,
    mode: u8,
) -> Result<task::testing::FakePublisherCallback<'a, Value>, task::EndpointError> {
    Ok(task::testing::FakePublisherCallback {
        publisher: bindings.take_publisher(key)?,
        body: source_body(mode),
        ordinal: 0,
        panic_on_flush: false,
    })
}

struct Transform {
    factor: u64,
}
#[task_callback]
impl Transform {
    fn run(
        &self,
        #[trigger(true)] input: RequiredInput<Value>,
        output: Output<Value>,
    ) -> Result<(), LoanError> {
        output.write(Value(input.0 * self.factor)).send();
        Ok(())
    }
}
fn recorded(log_source: bool) -> ReplayLog {
    let bytes = Bytes::default();
    {
        let mut source = ChannelPlan::new("source");
        let mut output = ChannelPlan::new("output");
        let a = source.publisher(4);
        let b = Transform::declare(&mut source, &mut output).unwrap();
        let source_capture = log_source.then(|| CapturePlan::declare(&mut source, 4));
        let output_capture = CapturePlan::declare(&mut output, 4);
        let storage = (source.allocate().unwrap(), output.allocate().unwrap());
        let source = storage.0.bind();
        let output = storage.1.bind();
        let mut graph = GraphBuilder::with_channel_names([storage.0.name(), storage.1.name()]);
        graph.add_callback("source", || Ok(source_callback(&source, &a, 0)?));
        graph.add_callback("transform", || {
            Ok(Transform { factor: 2 }.bind(b, &source, &output)?)
        });
        let graph = graph.build().unwrap();
        let log_plan = ExecutionLogPlan::for_graph(&graph, ExecutionLogOptions::new(8, 2)).unwrap();
        let log_storage = log_plan.allocate().unwrap();
        let (mut graph, metadata, log_captures) = log_storage.attach(graph).unwrap();
        graph.configure_schedules(|_, schedule| {
            *schedule = task_graph::CallbackSchedule {
                run_on_start: true,
                ..task_graph::CallbackSchedule::periodic(std::time::Duration::from_nanos(10))
                    .with_execution_duration(std::time::Duration::ZERO)
            };
        });
        let mut graph = testing::UnitTestExecutor::new_with_graph(graph, Default::default());
        let mut captures = vec![output_capture.bind(&output).unwrap()];
        if let Some(capture) = source_capture {
            captures.push(capture.bind(&source).unwrap());
        }
        captures.extend(log_captures);
        let log = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            captures,
        )
        .with_execution_log(metadata)
        .unwrap();
        graph.run_until(at(11), 32).unwrap();
        log.finish().unwrap();
    }
    let reader =
        logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap();
    ReplayLog::from_reader(&reader).unwrap()
}
fn replay<R>(
    log: ReplayLog,
    factor: u64,
    mode: u8,
    policy: DivergencePolicy,
    run: impl for<'a> FnOnce(ExactReplayExecutor<'a>) -> R,
) -> R {
    let mut source = ChannelPlan::new("source");
    let mut output = ChannelPlan::new("output");
    let a = source.publisher(4);
    let b = Transform::declare(&mut source, &mut output).unwrap();
    let hydration = ReplayInputPlan::declare(&mut source, b.input_key()).unwrap();
    let storage = (source.allocate().unwrap(), output.allocate().unwrap());
    let source = storage.0.bind();
    let output = storage.1.bind();
    let mut bindings = ReplayBindings::new();
    bindings
        .add_input("transform", 0, hydration.bind(&source).unwrap())
        .unwrap();
    bindings
        .add_output(
            "source",
            0,
            source
                .configure_publisher(&a, |publisher| OutputCapture::native(publisher, 8))
                .unwrap(),
        )
        .unwrap();
    bindings
        .add_output(
            "transform",
            0,
            output
                .configure_publisher(b.output_key(), |publisher| {
                    OutputCapture::native(publisher, 8)
                })
                .unwrap(),
        )
        .unwrap();
    let mut graph = GraphBuilder::with_channel_names([storage.0.name(), storage.1.name()]);
    // Resolve recorded indices by name, not by current graph insertion order.
    graph.add_callback("transform", || {
        Ok(Transform { factor }.bind(b, &source, &output)?)
    });
    graph.add_callback("source", || Ok(source_callback(&source, &a, mode)?));
    run(ExactReplayExecutor::with_config(
        graph.build().unwrap(),
        log,
        bindings,
        ExactReplayConfig {
            divergence_policy: policy,
            max_mismatch_details: 1,
        },
    )
    .unwrap())
}
#[test]
fn logged_and_reproduced_channels_replay_nonclone_payloads() {
    for logged in [true, false] {
        replay(
            recorded(logged),
            2,
            0,
            DivergencePolicy::Strict,
            |mut replay| {
                let report =
                    std::thread::scope(|scope| scope.spawn(move || replay.run()).join().unwrap())
                        .unwrap();
                assert!(report.is_exact(), "{report:?}");
                assert_eq!(report.consumed_executions(), 4);
                assert_eq!(report.logged_count(), if logged { 6 } else { 2 });
                assert_eq!(report.reproduced_count(), if logged { 0 } else { 4 });
            },
        );
    }
}

#[test]
fn publisher_translation_does_not_mask_invalid_publisher_time_or_batch_indices() {
    for mode in [5, 6, 7] {
        replay(
            recorded(false),
            2,
            mode,
            DivergencePolicy::Strict,
            |mut executor| {
                assert!(matches!(
                    executor.run(),
                    Err(ExactReplayStepError::Divergence(_))
                ));
                assert!(!executor.replay_report().is_exact());
            },
        );
    }
    replay(
        recorded(false),
        2,
        5,
        DivergencePolicy::BestEffort,
        |mut executor| {
            let report = executor.run().unwrap();
            assert!(!report.is_exact());
            assert_eq!(report.mismatch_count(), 2);
            assert_eq!(report.gap_count(), 2);
            assert_eq!(report.reproduced_count(), 0);
            assert_eq!(report.details().len(), 1);
            assert_eq!(
                report.details()[0].kind,
                exact_replay_executor::ReplayDetailKind::Mismatch
            );
            assert!(report.details()[0].publication.is_some());
            assert!(report.omitted_details() > 0);
        },
    );
}
#[test]
fn strict_stops_and_best_effort_collects_byte_count_and_missing_output_mismatches() {
    for (factor, mode) in [(3, 0), (2, 1), (2, 2)] {
        replay(
            recorded(true),
            factor,
            mode,
            DivergencePolicy::Strict,
            |mut replay| {
                assert!(matches!(
                    replay.run(),
                    Err(ExactReplayStepError::Divergence(_))
                ));
                assert!(matches!(replay.step(), Err(ExactReplayStepError::Poisoned)));
                assert!(!replay.replay_report().is_exact());
            },
        );
        replay(
            recorded(true),
            factor,
            mode,
            DivergencePolicy::BestEffort,
            |mut replay| {
                let report = replay.run().unwrap();
                assert_eq!(report.consumed_executions(), 4);
                assert_eq!(report.mismatch_count(), 2);
                assert_eq!(report.mismatch_details().len(), 1);
                assert_eq!(report.details().len(), 1);
                let detail = &report.details()[0];
                assert_eq!(
                    detail.kind,
                    exact_replay_executor::ReplayDetailKind::Mismatch
                );
                assert!(
                    report
                        .intern_tables()
                        .callbacks
                        .try_lookup_by_id(detail.callback)
                        .is_some()
                );
                assert!(
                    report
                        .intern_tables()
                        .channels
                        .try_lookup_by_id(detail.channel.unwrap())
                        .is_some()
                );
                assert!(detail.endpoint.is_some());
                assert!(detail.publication.is_some());
                assert_eq!(report.omitted_details(), 1);
                assert!(!report.is_exact());
            },
        );
    }
}
#[test]
fn panic_is_terminal_even_in_best_effort_and_stop_is_scope_local_metadata() {
    for mode in [3, 4] {
        replay(
            recorded(true),
            2,
            mode,
            DivergencePolicy::BestEffort,
            |mut replay| {
                assert!(matches!(
                    replay.run(),
                    Err(ExactReplayStepError::Callback { .. })
                ));
                assert!(matches!(replay.step(), Err(ExactReplayStepError::Poisoned)));
                assert_eq!(replay.replay_report().error_count(), 1);
                assert_eq!(
                    replay.replay_report().details()[0].kind,
                    exact_replay_executor::ReplayDetailKind::Error
                );
            },
        );
    }
    replay(
        recorded(true),
        2,
        0,
        DivergencePolicy::Strict,
        |mut replay| {
            replay.stop_signal().request_stop();
            assert!(!replay.run().unwrap().is_exact());
            assert_eq!(replay.replay_report().consumed_executions(), 0);
        },
    );
}

fn log_from(
    descriptor: task::execution_log_schema::ExecutionDescriptor,
    records: Vec<task::execution_log_schema::ExecutionRecord>,
    payloads: Vec<(&str, i64, Vec<u8>)>,
) -> Result<ReplayLog, ExactReplayStepError> {
    use task::{execution_log_schema::*, message::MessageHeader};
    let mut tables = InternTables::default();
    for callback in &descriptor.callbacks {
        tables.callbacks.intern(&callback.name);
        for endpoint in &callback.endpoints {
            tables.channels.intern(&endpoint.channel);
        }
    }
    let mut entries: Vec<_> = payloads
        .into_iter()
        .map(|(channel, time, serialized_body)| logging::OwnedLogEntry {
            channel_name: channel.into(),
            header: MessageHeader::new(at(time)),
            serialized_body,
        })
        .collect();
    entries.extend(records.into_iter().map(|record| logging::OwnedLogEntry {
        channel_name: EXECUTION_LOG_CHANNEL.into(),
        header: MessageHeader::new(record.execution_time),
        serialized_body: serde_json::to_vec(&record).unwrap(),
    }));
    ReplayLog::from_sorted(
        logging::SortedLogStreamReader::from_entries(
            entries,
            HashMap::from([
                (
                    EXECUTION_LOG_DESCRIPTOR_ARTIFACT.into(),
                    serde_json::to_vec(&descriptor).unwrap(),
                ),
                (
                    INTERN_TABLES_ARTIFACT.into(),
                    serde_json::to_vec(&tables).unwrap(),
                ),
            ]),
        )
        .unwrap(),
    )
}
#[test]
fn missing_and_ambiguous_payloads_and_failed_records_cannot_claim_exactness() {
    use task::execution_log_schema::*;
    let original = recorded(true);
    let descriptor = original.descriptor_ref().clone();
    assert!(
        log_from(
            descriptor.clone(),
            vec![],
            vec![("source", 0, b"1".to_vec()), ("source", 0, b"2".to_vec())]
        )
        .is_err()
    );
    let mut record = ExecutionRecord {
        callback_id: task::string_interner::CallbackId::from_index(1).unwrap(),
        execution_time: at(0),
        body_duration_ns: 1,
        inputs: bounded([LoggedMessage {
            ordinal: 0,
            header: task::message::MessageHeader::new(at(0)),
        }]),
        outputs: bounded([]),
        iox2_events: bounded([]),
        output_iox2_events: bounded([]),
        outcome: Outcome::Committed,
        truncated: 0,
    };
    replay(
        log_from(descriptor.clone(), vec![record.clone()], vec![]).unwrap(),
        2,
        0,
        DivergencePolicy::Strict,
        |mut replay| {
            assert!(matches!(
                replay.run(),
                Err(ExactReplayStepError::Gap { .. })
            ));
        },
    );
    replay(
        log_from(descriptor.clone(), vec![record.clone()], vec![]).unwrap(),
        2,
        0,
        DivergencePolicy::BestEffort,
        |mut replay| {
            let step = replay.step().unwrap().unwrap();
            assert!(!step.executed);
            let report = replay.replay_report();
            assert_eq!(
                report.details()[0].kind,
                exact_replay_executor::ReplayDetailKind::Gap
            );
            assert!(report.details()[0].publication.is_some());
            assert!(!replay.run().unwrap().is_exact());
        },
    );
    record.outcome = Outcome::CommitPanicked;
    assert!(log_from(descriptor, vec![record], vec![]).is_err());
}

#[test]
fn invalid_descriptors_bindings_and_ambiguous_publications_fail_construction() {
    use task::execution_log_schema::*;
    let empty = logging::log_file_json::JsonLogFileReader::from_reader(b"".as_slice()).unwrap();
    assert!(matches!(
        ReplayLog::from_reader(&empty),
        Err(ExactReplayStepError::InvalidLog(_))
    ));
    assert!(matches!(
        ExactReplayExecutor::new(
            GraphBuilder::new().build().unwrap(),
            recorded(true),
            ReplayBindings::new()
        ),
        Err(ExactReplayStepError::Setup(_))
    ));

    let mut source = ChannelPlan::new("source");
    let mut output = ChannelPlan::new("output");
    let a = source.publisher(4);
    let b = Transform::declare(&mut source, &mut output).unwrap();
    let storage = (source.allocate().unwrap(), output.allocate().unwrap());
    let source = storage.0.bind();
    let output = storage.1.bind();
    let mut graph = GraphBuilder::new();
    graph.add_callback("source", || Ok(source_callback(&source, &a, 0)?));
    graph.add_callback("transform", || {
        Ok(Transform { factor: 2 }.bind(b, &source, &output)?)
    });
    let error = ExactReplayExecutor::new(
        graph.build().unwrap(),
        recorded(true),
        ReplayBindings::new(),
    )
    .err()
    .unwrap();
    assert!(
        matches!(error, ExactReplayStepError::Setup(reason) if reason.contains("missing output capture"))
    );

    let descriptor = recorded(true).descriptor_ref().clone();
    let record = ExecutionRecord {
        callback_id: task::string_interner::CallbackId::from_index(0).unwrap(),
        execution_time: at(0),
        body_duration_ns: 1,
        inputs: bounded([]),
        iox2_events: bounded([]),
        output_iox2_events: bounded([]),
        outputs: bounded([LoggedMessage {
            ordinal: 0,
            header: task::message::MessageHeader::new(at(0)),
        }]),
        outcome: Outcome::Committed,
        truncated: 0,
    };
    assert!(
        matches!(log_from(descriptor, vec![record.clone(), record], vec![]), Err(ExactReplayStepError::InvalidLog(reason)) if reason.contains("ambiguous publication"))
    );
}

#[test]
fn publisher_lookup_and_batch_headers_are_validated_before_execution() {
    use task::{execution_log_schema::*, message::MessageHeader};
    let descriptor = recorded(true).descriptor_ref().clone();
    for header in [
        MessageHeader {
            published_at: at(0),
            publisher_index: 1,
            batch_index: 0,
        },
        MessageHeader {
            published_at: at(0),
            publisher_index: 0,
            batch_index: 1,
        },
        MessageHeader::new(at(1)),
    ] {
        let record = ExecutionRecord {
            callback_id: task::string_interner::CallbackId::from_index(0).unwrap(),
            execution_time: at(0),
            body_duration_ns: 0,
            inputs: bounded([]),
            iox2_events: bounded([]),
            output_iox2_events: bounded([]),
            outputs: bounded([LoggedMessage { ordinal: 0, header }]),
            outcome: Outcome::Committed,
            truncated: 0,
        };
        assert!(matches!(
            log_from(descriptor.clone(), vec![record], vec![]),
            Err(ExactReplayStepError::InvalidLog(_))
        ));
    }
    let mut duplicate = descriptor.clone();
    let mut output = duplicate.callbacks[0].endpoints[0].clone();
    output.ordinal = 1;
    duplicate.callbacks[0].endpoints.push(output);
    assert!(
        matches!(log_from(duplicate, vec![], vec![]), Err(ExactReplayStepError::InvalidLog(reason)) if reason.contains("duplicate publisher index"))
    );
    let mut absent = descriptor;
    absent.callbacks[0].endpoints[0].publisher_index = None;
    assert!(matches!(
        log_from(absent, vec![], vec![]),
        Err(ExactReplayStepError::InvalidLog(_))
    ));
}

#[test]
fn incomplete_logs_are_rejected_even_when_the_surviving_trace_is_valid() {
    use logging::{LogFileWriter, incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT};
    for body in [
        b"null".as_slice(),
        b"{}",
        b"{\"errors\":[\"dropped records\"]}",
    ] {
        let bytes = Bytes::default();
        let mut writer = logging::log_file_json::JsonLogFileWriter::new(bytes.clone());
        writer
            .write_artifact(
                task::execution_log_schema::EXECUTION_LOG_DESCRIPTOR_ARTIFACT,
                &serde_json::to_vec(recorded(true).descriptor_ref()).unwrap(),
            )
            .unwrap();
        writer
            .write_artifact(RECORDING_INCOMPLETENESS_ARTIFACT, body)
            .unwrap();
        // Later snapshots cannot clear the sticky marker.
        writer
            .write_artifact(RECORDING_INCOMPLETENESS_ARTIFACT, b"{}")
            .unwrap();
        let raw = bytes.0.lock().unwrap().clone();
        let reader =
            logging::log_file_json::JsonLogFileReader::from_reader(raw.as_slice()).unwrap();
        assert!(
            matches!(ReplayLog::from_reader(&reader), Err(ExactReplayStepError::InvalidLog(reason)) if reason.contains("incomplete"))
        );
        #[cfg(not(miri))]
        let reader = logging::SortedLogStreamReader::from_reader(raw.as_slice(), 1).unwrap();
        #[cfg(miri)]
        let reader = logging::SortedLogStreamReader::from_entries(
            vec![],
            HashMap::from([(RECORDING_INCOMPLETENESS_ARTIFACT.into(), body.to_vec())]),
        )
        .unwrap();
        assert!(
            matches!(ReplayLog::from_sorted(reader), Err(ExactReplayStepError::InvalidLog(reason)) if reason.contains("incomplete"))
        );
    }
}

struct TwoInputs;
#[task_callback]
impl TwoInputs {
    fn run(
        &self,
        #[trigger(true)] first: RequiredInput<u64>,
        #[trigger(true)] second: RequiredInput<u64>,
        output: Output<u64>,
    ) -> Result<(), LoanError> {
        output.write(*first * 10 + *second).send();
        Ok(())
    }
}
#[test]
fn subscribers_on_one_channel_restore_distinct_recorded_snapshots() {
    let bytes = Bytes::default();
    {
        let mut shared = ChannelPlan::new("shared");
        let mut result = ChannelPlan::new("result");
        let declaration = TwoInputsDeclaration::from_keys(
            shared.subscriber(1),
            shared.subscriber(1),
            result.publisher(1),
        );
        let first = shared.publisher(1);
        let second = shared.publisher(1);
        shared
            .restrict_subscriber_sources(declaration.first_key(), std::slice::from_ref(&first))
            .unwrap();
        shared
            .restrict_subscriber_sources(declaration.second_key(), std::slice::from_ref(&second))
            .unwrap();
        let capture = CapturePlan::declare(&mut shared, 2);
        let out = CapturePlan::declare(&mut result, 1);
        let storage = (shared.allocate().unwrap(), result.allocate().unwrap());
        let shared = storage.0.bind();
        let result = storage.1.bind();
        let mut graph = GraphBuilder::new();
        graph.add_callback("pair", || {
            Ok(TwoInputs.bind(declaration, &shared, &shared, &result)?)
        });
        let graph = graph.build().unwrap();
        let log_plan = ExecutionLogPlan::for_graph(&graph, ExecutionLogOptions::new(1, 1)).unwrap();
        let log_storage = log_plan.allocate().unwrap();
        let (mut graph, metadata, log_captures) = log_storage.attach(graph).unwrap();
        graph.configure_schedules(|_, schedule| {
            *schedule = task_graph::CallbackSchedule::on_start()
                .with_execution_duration(std::time::Duration::ZERO);
        });
        let mut graph = testing::UnitTestExecutor::new_with_graph(
            graph,
            testing::UnitTestExecutorConfig {
                start_time: at(20),
                ..Default::default()
            },
        );
        let mut captures = vec![capture.bind(&shared).unwrap(), out.bind(&result).unwrap()];
        captures.extend(log_captures);
        let log = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            captures,
        )
        .with_execution_log(metadata)
        .unwrap();
        logging::ReplaySource::native(shared.take_publisher(&first).unwrap())
            .inject(task::message::MessageHeader::new(at(0)), b"1")
            .unwrap();
        logging::ReplaySource::native(shared.take_publisher(&second).unwrap())
            .inject(task::message::MessageHeader::new(at(10)), b"2")
            .unwrap();
        graph.run_until_idle(8).unwrap();
        log.finish().unwrap();
    }
    let log = ReplayLog::from_reader(
        &logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap(),
    )
    .unwrap();
    let mut shared = ChannelPlan::new("shared");
    let mut result = ChannelPlan::new("result");
    let declaration = TwoInputsDeclaration::from_keys(
        shared.subscriber(1),
        shared.subscriber(1),
        result.publisher(1),
    );
    let first = ReplayInputPlan::declare(&mut shared, declaration.first_key()).unwrap();
    let second = ReplayInputPlan::declare(&mut shared, declaration.second_key()).unwrap();
    let storage = (shared.allocate().unwrap(), result.allocate().unwrap());
    let shared = storage.0.bind();
    let result = storage.1.bind();
    let mut endpoints = ReplayBindings::new();
    endpoints
        .add_input("pair", 0, first.bind(&shared).unwrap())
        .unwrap();
    endpoints
        .add_input("pair", 1, second.bind(&shared).unwrap())
        .unwrap();
    endpoints
        .add_output(
            "pair",
            0,
            result
                .configure_publisher(declaration.output_key(), |p| OutputCapture::native(p, 1))
                .unwrap(),
        )
        .unwrap();
    let mut graph = GraphBuilder::new();
    graph.add_callback("pair", || {
        Ok(TwoInputs.bind(declaration, &shared, &shared, &result)?)
    });
    assert!(
        ExactReplayExecutor::new(graph.build().unwrap(), log, endpoints)
            .unwrap()
            .run()
            .unwrap()
            .is_exact()
    );
}

struct TwoOutputs {
    swapped: bool,
}
#[task_callback]
impl TwoOutputs {
    fn run(&self, first: Output<u64>, second: Output<u64>) -> Result<(), LoanError> {
        if self.swapped {
            first.write(7).send();
            second.write(42).send();
        } else {
            first.write(42).send();
            second.write(7).send();
        }
        Ok(())
    }
}
#[test]
fn outputs_on_one_channel_are_attributed_to_the_actual_publisher() {
    let bytes = Bytes::default();
    {
        let mut plan = ChannelPlan::new("shared");
        let declaration = TwoOutputsDeclaration::from_keys(plan.publisher(1), plan.publisher(1));
        let capture = CapturePlan::declare(&mut plan, 2);
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind();
        let mut graph = GraphBuilder::new();
        graph.add_callback("pair", || {
            Ok(TwoOutputs { swapped: false }.bind(declaration, &bindings, &bindings)?)
        });
        let graph = graph.build().unwrap();
        let log_plan = ExecutionLogPlan::for_graph(&graph, ExecutionLogOptions::new(1, 1)).unwrap();
        let log_storage = log_plan.allocate().unwrap();
        let (mut graph, metadata, log_captures) = log_storage.attach(graph).unwrap();
        graph.configure_schedules(|_, schedule| {
            *schedule = task_graph::CallbackSchedule::on_start()
                .with_execution_duration(std::time::Duration::ZERO);
        });
        let mut graph = testing::UnitTestExecutor::new_with_graph(graph, Default::default());
        let mut captures = vec![capture.bind(&bindings).unwrap()];
        captures.extend(log_captures);
        let log = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            captures,
        )
        .with_execution_log(metadata)
        .unwrap();
        graph.run_until_idle(8).unwrap();
        log.finish().unwrap();
    }
    let log = ReplayLog::from_reader(
        &logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap(),
    )
    .unwrap();
    let mut plan = ChannelPlan::new("shared");
    let second = plan.publisher(1);
    let first = plan.publisher(1);
    let declaration = TwoOutputsDeclaration::from_keys(first, second);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind();
    let mut endpoints = ReplayBindings::new();
    endpoints
        .add_output(
            "pair",
            0,
            bindings
                .configure_publisher(declaration.first_key(), |p| OutputCapture::native(p, 1))
                .unwrap(),
        )
        .unwrap();
    endpoints
        .add_output(
            "pair",
            1,
            bindings
                .configure_publisher(declaration.second_key(), |p| OutputCapture::native(p, 1))
                .unwrap(),
        )
        .unwrap();
    let mut graph = GraphBuilder::new();
    graph.add_callback("pair", || {
        Ok(TwoOutputs { swapped: true }.bind(declaration, &bindings, &bindings)?)
    });
    let report = ExactReplayExecutor::with_config(
        graph.build().unwrap(),
        log,
        endpoints,
        ExactReplayConfig {
            divergence_policy: DivergencePolicy::BestEffort,
            ..Default::default()
        },
    )
    .unwrap()
    .run()
    .unwrap();
    assert_eq!(report.mismatch_count(), 2);
    assert_eq!(report.exact_reproduction_ratio(), 0.0);
}

struct Forward;
type SourcePtr<'a> = task::ArenaReaderPtr<'a, task::message::Message<Value>>;
#[task_callback]
impl Forward {
    fn run<'graph_storage>(
        &self,
        #[trigger(true)] mut input: task::Input<'_, 'graph_storage, Value, task::Forwardable>,
        #[forwards(source = input)] output: Output<
            '_,
            'graph_storage,
            task::Forwarded<bool, SourcePtr<'graph_storage>>,
        >,
    ) -> Result<(), LoanError> {
        if let Some(value) = input.pop() {
            output.write(task::Forwarded::new(true, value)).send();
        }
        Ok(())
    }
}
struct Consume;
#[task_callback]
impl Consume {
    fn run(
        &self,
        #[trigger(true)] input: RequiredInput<task::Forwarded<bool, SourcePtr<'_>>>,
        output: Output<Value>,
    ) -> Result<(), LoanError> {
        output
            .write(Value(
                input.forwarded_messages.message.0 + if input.message { 10 } else { 100 },
            ))
            .send();
        Ok(())
    }
}
#[test]
fn forwarded_inputs_resolve_logged_and_reproduced_nonclone_sources() {
    for logged_source in [false, true] {
        for logged_forward in [false, true] {
            let bytes = Bytes::default();
            {
                let mut source = ChannelPlan::new("source");
                let mut forward = ChannelPlan::new("forward");
                let mut result = ChannelPlan::new("result");
                let a = source.publisher(4);
                let b = Forward::declare(&mut source, &mut forward).unwrap();
                let c = Consume::declare(&mut forward, &mut result).unwrap();
                source.reserve_retained(&a, 4).unwrap();
                let source_log = logged_source.then(|| CapturePlan::declare(&mut source, 2));
                let forward_log = logged_forward.then(|| CapturePlan::declare(&mut forward, 2));
                let result_log = CapturePlan::declare(&mut result, 2);
                let storage = (
                    source.allocate().unwrap(),
                    forward.allocate().unwrap(),
                    result.allocate().unwrap(),
                );
                let source = storage.0.bind();
                let forward = storage.1.bind();
                let result = storage.2.bind();
                let mut graph = GraphBuilder::new();
                graph.add_callback("source", || Ok(source_callback(&source, &a, 0)?));
                graph.add_callback("forward", || Ok(Forward.bind(b, &source, &forward)?));
                graph.add_callback("consume", || Ok(Consume.bind(c, &forward, &result)?));
                let graph = graph.build().unwrap();
                let log_plan =
                    ExecutionLogPlan::for_graph(&graph, ExecutionLogOptions::new(8, 1)).unwrap();
                let log_storage = log_plan.allocate().unwrap();
                let (mut graph, metadata, log_captures) = log_storage.attach(graph).unwrap();
                graph.configure_schedules(|_, schedule| {
                    *schedule = task_graph::CallbackSchedule {
                        run_on_start: true,
                        ..task_graph::CallbackSchedule::periodic(std::time::Duration::from_nanos(
                            10,
                        ))
                        .with_execution_duration(std::time::Duration::ZERO)
                    };
                });
                let mut graph =
                    testing::UnitTestExecutor::new_with_graph(graph, Default::default());
                let mut captures = vec![result_log.bind(&result).unwrap()];
                if let Some(capture) = source_log {
                    captures.push(capture.bind(&source).unwrap());
                }
                if let Some(capture) = forward_log {
                    captures.push(capture.bind(&forward).unwrap());
                }
                captures.extend(log_captures);
                let log = LogSession::new(
                    logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
                    captures,
                )
                .with_execution_log(metadata)
                .unwrap();
                graph.run_until(at(11), 32).unwrap();
                log.finish().unwrap();
            }
            let log = ReplayLog::from_reader(
                &logging::log_file_json::JsonLogFileReader::from_reader(
                    bytes.0.lock().unwrap().as_slice(),
                )
                .unwrap(),
            )
            .unwrap();
            let mut source = ChannelPlan::new("source");
            let mut forward = ChannelPlan::new("forward");
            let mut result = ChannelPlan::new("result");
            let a = source.publisher(4);
            let b = Forward::declare(&mut source, &mut forward).unwrap();
            let c = Consume::declare(&mut forward, &mut result).unwrap();
            let cache = exact_replay_executor::SourceCachePlan::declare(
                &mut source,
                log.source_capacity("source"),
            )
            .unwrap();
            let hydrate_b = ReplayInputPlan::declare(&mut source, b.input_key()).unwrap();
            let hydrate_c = ReplayInputPlan::declare(&mut forward, c.input_key()).unwrap();
            let storage = (
                source.allocate().unwrap(),
                forward.allocate().unwrap(),
                result.allocate().unwrap(),
            );
            let source = storage.0.bind();
            let forward = storage.1.bind();
            let result = storage.2.bind();
            let cache = cache.bind(&source).unwrap();
            let mut endpoints = ReplayBindings::new();
            let loader = cache.clone();
            endpoints
                .add_cache("source", move |h, b| loader.load(h, b))
                .unwrap();
            endpoints
                .add_input("forward", 0, hydrate_b.bind(&source).unwrap())
                .unwrap();
            endpoints
                .add_input(
                    "consume",
                    0,
                    hydrate_c
                        .bind_with_decoder(&forward, move |bytes| {
                            <task::Forwarded<bool,SourcePtr<'_>> as task::loggable::Loggable>::deserialize_with_ctx(bytes,&cache)
                        })
                        .unwrap(),
                )
                .unwrap();
            endpoints
                .add_output(
                    "source",
                    0,
                    source
                        .configure_publisher(&a, |p| OutputCapture::native(p, 4))
                        .unwrap(),
                )
                .unwrap();
            endpoints
                .add_output(
                    "forward",
                    0,
                    forward
                        .configure_publisher(b.output_key(), |p| OutputCapture::native(p, 4))
                        .unwrap(),
                )
                .unwrap();
            endpoints
                .add_output(
                    "consume",
                    0,
                    result
                        .configure_publisher(c.output_key(), |p| OutputCapture::native(p, 4))
                        .unwrap(),
                )
                .unwrap();
            let mut graph = GraphBuilder::new();
            graph.add_callback("source", || Ok(source_callback(&source, &a, 0)?));
            graph.add_callback("forward", || Ok(Forward.bind(b, &source, &forward)?));
            graph.add_callback("consume", || Ok(Consume.bind(c, &forward, &result)?));
            let report = ExactReplayExecutor::new(graph.build().unwrap(), log, endpoints)
                .unwrap()
                .run()
                .unwrap();
            assert!(report.is_exact(), "{report:?}");
            assert_eq!(report.consumed_executions(), 6);
            assert_eq!(
                report.reproduced_count(),
                (if logged_source { 0 } else { 4 }) + (if logged_forward { 0 } else { 4 })
            );
        }
    }
}
