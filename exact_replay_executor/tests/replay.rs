use exact_replay_executor::{
    DivergencePolicy, ExactReplayConfig, ExactReplayExecutor, ReplayBindings, ReplayError,
    ReplayInputPlan, ReplayLog,
};
use logging::{CapturePlan, ExecutionRecorder, LogSession, PortCapture};
use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex},
};
use task::{
    ChannelPlan, GraphBuilder, GraphPlan, LoanError, Publisher, RequiredInput, time::FrameworkTime,
};
use task_macros::task_callback;

fn at(n: i64) -> FrameworkTime {
    FrameworkTime::from_nanoseconds(n)
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
struct Source {
    next: u64,
    mode: u8,
}
#[task_callback]
impl Source {
    fn run(
        &mut self,
        #[capacity(4)] output: &mut Publisher<Value>,
        context: &task::Context,
    ) -> Result<(), LoanError> {
        let mut names = task::string_interner::CallbackNameInterner::new();
        assert_eq!(
            context.callback_names.lookup_by_value("source"),
            Some(names.intern("source"))
        );
        if self.mode == 3 {
            panic!("callback panic");
        }
        if self.mode != 2 {
            output.publish(Value(if self.mode == 4 { u64::MAX } else { self.next }))?;
        }
        if self.mode == 1 {
            output.publish(Value(999))?;
        }
        self.next += 1;
        Ok(())
    }
}
struct Transform {
    factor: u64,
}
#[task_callback]
impl Transform {
    fn run(
        &self,
        input: RequiredInput<Value>,
        output: &mut Publisher<Value>,
    ) -> Result<(), LoanError> {
        output.publish(Value(input.0 * self.factor))
    }
}
fn recorded(log_source: bool) -> ReplayLog {
    let bytes = Bytes::default();
    {
        let mut source = ChannelPlan::new("source");
        let mut output = ChannelPlan::new("output");
        let a = Source::declare(&mut source).unwrap();
        let b = Transform::declare(&mut source, &mut output).unwrap();
        let source_capture = log_source.then(|| CapturePlan::declare(&mut source, 4));
        let output_capture = CapturePlan::declare(&mut output, 4);
        let storage = GraphPlan::new((source, output)).allocate().unwrap();
        let source = storage.channels().0.build();
        let output = storage.channels().1.build();
        let mut graph = GraphBuilder::with_storage(&storage);
        graph.add_callback("source", || {
            Ok(Source { next: 1, mode: 0 }.bind(a, &source)?)
        });
        graph.add_callback("transform", || {
            Ok(Transform { factor: 2 }.bind(b, &source, &output)?)
        });
        let recorder = ExecutionRecorder::new(8);
        let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
        let mut captures = vec![output_capture.bind(&output).unwrap()];
        if let Some(capture) = source_capture {
            captures.push(capture.bind(&source).unwrap());
        }
        let log = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            captures,
        )
        .with_recording(recorder)
        .unwrap();
        graph.step(at(0)).unwrap();
        graph.step(at(10)).unwrap();
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
    let a = Source::declare(&mut source).unwrap();
    let b = Transform::declare(&mut source, &mut output).unwrap();
    let hydration = ReplayInputPlan::declare(&mut source, b.input_key()).unwrap();
    let storage = GraphPlan::new((source, output)).allocate().unwrap();
    let source = storage.channels().0.build();
    let output = storage.channels().1.build();
    let mut bindings = ReplayBindings::new();
    bindings
        .add_input("transform", 0, hydration.bind(&source).unwrap())
        .unwrap();
    bindings
        .add_output(
            "source",
            0,
            source
                .configure_publisher(a.output_key(), |publisher| {
                    PortCapture::native(publisher, 8)
                })
                .unwrap(),
        )
        .unwrap();
    bindings
        .add_output(
            "transform",
            0,
            output
                .configure_publisher(b.output_key(), |publisher| {
                    PortCapture::native(publisher, 8)
                })
                .unwrap(),
        )
        .unwrap();
    let mut graph = GraphBuilder::with_storage(&storage);
    // Resolve recorded indices by name, not by current graph insertion order.
    graph.add_callback("transform", || {
        Ok(Transform { factor }.bind(b, &source, &output)?)
    });
    graph.add_callback("source", || Ok(Source { next: 1, mode }.bind(a, &source)?));
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
fn strict_stops_and_best_effort_collects_byte_count_and_missing_output_mismatches() {
    for (factor, mode) in [(3, 0), (2, 1), (2, 2)] {
        replay(
            recorded(true),
            factor,
            mode,
            DivergencePolicy::Strict,
            |mut replay| {
                assert!(matches!(replay.run(), Err(ReplayError::Divergence(_))));
                assert!(matches!(replay.step(), Err(ReplayError::Poisoned)));
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
                assert!(matches!(replay.run(), Err(ReplayError::Callback { .. })));
                assert!(matches!(replay.step(), Err(ReplayError::Poisoned)));
                assert_eq!(replay.replay_report().error_count(), 1);
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
    descriptor: task::recording::ExecutionDescriptor,
    records: Vec<task::recording::ExecutionRecord>,
    payloads: Vec<(&str, i64, Vec<u8>)>,
) -> Result<ReplayLog, ReplayError> {
    use task::{message::MessageHeader, recording::*};
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
            HashMap::from([(
                EXECUTION_LOG_DESCRIPTOR_ARTIFACT.into(),
                serde_json::to_vec(&descriptor).unwrap(),
            )]),
        )
        .unwrap(),
    )
}
#[test]
fn missing_and_ambiguous_payloads_and_failed_records_cannot_claim_exactness() {
    use task::recording::*;
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
        callback_index: 1,
        execution_time: at(0),
        body_duration_ns: 1,
        inputs: vec![LoggedMessage {
            ordinal: 0,
            header: task::message::MessageHeader::new(at(0)),
        }],
        outputs: vec![],
        events: vec![],
        output_events: vec![],
        outcome: Outcome::Committed,
    };
    replay(
        log_from(descriptor.clone(), vec![record.clone()], vec![]).unwrap(),
        2,
        0,
        DivergencePolicy::Strict,
        |mut replay| {
            assert!(matches!(replay.run(), Err(ReplayError::Gap { .. })));
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
            assert!(!replay.run().unwrap().is_exact());
        },
    );
    record.outcome = Outcome::CommitPanicked;
    assert!(log_from(descriptor, vec![record], vec![]).is_err());
}

#[test]
fn invalid_descriptors_bindings_and_ambiguous_publications_fail_construction() {
    use task::recording::*;
    let empty = logging::log_file_json::JsonLogFileReader::from_reader(b"".as_slice()).unwrap();
    assert!(matches!(
        ReplayLog::from_reader(&empty),
        Err(ReplayError::InvalidLog(_))
    ));
    assert!(matches!(
        ExactReplayExecutor::new(
            GraphBuilder::new().build().unwrap(),
            recorded(true),
            ReplayBindings::new()
        ),
        Err(ReplayError::Setup(_))
    ));

    let mut source = ChannelPlan::new("source");
    let mut output = ChannelPlan::new("output");
    let a = Source::declare(&mut source).unwrap();
    let b = Transform::declare(&mut source, &mut output).unwrap();
    let storage = GraphPlan::new((source, output)).allocate().unwrap();
    let source = storage.channels().0.build();
    let output = storage.channels().1.build();
    let mut graph = GraphBuilder::new();
    graph.add_callback("source", || {
        Ok(Source { next: 1, mode: 0 }.bind(a, &source)?)
    });
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
        matches!(error, ReplayError::Setup(reason) if reason.contains("missing output capture"))
    );

    let descriptor = recorded(true).descriptor_ref().clone();
    let record = ExecutionRecord {
        callback_index: 0,
        execution_time: at(0),
        body_duration_ns: 1,
        inputs: vec![],
        events: vec![],
        output_events: vec![],
        outputs: vec![LoggedMessage {
            ordinal: 0,
            header: task::message::MessageHeader::new(at(0)),
        }],
        outcome: Outcome::Committed,
    };
    assert!(
        matches!(log_from(descriptor, vec![record.clone(), record], vec![]), Err(ReplayError::InvalidLog(reason)) if reason.contains("ambiguous publication"))
    );
}

#[test]
fn publisher_lookup_and_batch_headers_are_validated_before_execution() {
    use task::{message::MessageHeader, recording::*};
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
            callback_index: 0,
            execution_time: at(0),
            body_duration_ns: 0,
            inputs: vec![],
            events: vec![],
            output_events: vec![],
            outputs: vec![LoggedMessage { ordinal: 0, header }],
            outcome: Outcome::Committed,
        };
        assert!(matches!(
            log_from(descriptor.clone(), vec![record], vec![]),
            Err(ReplayError::InvalidLog(_))
        ));
    }
    let mut duplicate = descriptor.clone();
    let mut output = duplicate.callbacks[0].endpoints[0].clone();
    output.ordinal = 1;
    duplicate.callbacks[0].endpoints.push(output);
    assert!(
        matches!(log_from(duplicate, vec![], vec![]), Err(ReplayError::InvalidLog(reason)) if reason.contains("duplicate publisher index"))
    );
    let mut absent = descriptor;
    absent.callbacks[0].endpoints[0].publisher_index = None;
    assert!(matches!(
        log_from(absent, vec![], vec![]),
        Err(ReplayError::InvalidLog(_))
    ));
}

struct TwoInputs;
#[task_callback]
impl TwoInputs {
    fn run(
        &self,
        first: RequiredInput<u64>,
        second: RequiredInput<u64>,
        output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        output.publish(*first * 10 + *second)
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
        let storage = GraphPlan::new((shared, result)).allocate().unwrap();
        let shared = storage.channels().0.build();
        let result = storage.channels().1.build();
        let mut graph = GraphBuilder::new();
        graph.add_callback("pair", || {
            Ok(TwoInputs.bind(declaration, &shared, &shared, &result)?)
        });
        let recorder = ExecutionRecorder::new(1);
        let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
        let log = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            vec![capture.bind(&shared).unwrap(), out.bind(&result).unwrap()],
        )
        .with_recording(recorder)
        .unwrap();
        logging::ReplaySource::native(shared.take_publisher(&first).unwrap())
            .inject(task::message::MessageHeader::new(at(0)), b"1")
            .unwrap();
        logging::ReplaySource::native(shared.take_publisher(&second).unwrap())
            .inject(task::message::MessageHeader::new(at(10)), b"2")
            .unwrap();
        graph.step(at(20)).unwrap();
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
    let storage = GraphPlan::new((shared, result)).allocate().unwrap();
    let shared = storage.channels().0.build();
    let result = storage.channels().1.build();
    let mut ports = ReplayBindings::new();
    ports
        .add_input("pair", 0, first.bind(&shared).unwrap())
        .unwrap();
    ports
        .add_input("pair", 1, second.bind(&shared).unwrap())
        .unwrap();
    ports
        .add_output(
            "pair",
            0,
            result
                .configure_publisher(declaration.output_key(), |p| PortCapture::native(p, 1))
                .unwrap(),
        )
        .unwrap();
    let mut graph = GraphBuilder::new();
    graph.add_callback("pair", || {
        Ok(TwoInputs.bind(declaration, &shared, &shared, &result)?)
    });
    assert!(
        ExactReplayExecutor::new(graph.build().unwrap(), log, ports)
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
    fn run(
        &self,
        first: &mut Publisher<u64>,
        second: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        if self.swapped {
            first.publish(7)?;
            second.publish(42)
        } else {
            first.publish(42)?;
            second.publish(7)
        }
    }
}
#[test]
fn outputs_on_one_channel_are_attributed_to_the_actual_publisher() {
    let bytes = Bytes::default();
    {
        let mut plan = ChannelPlan::new("shared");
        let declaration = TwoOutputsDeclaration::from_keys(plan.publisher(1), plan.publisher(1));
        let capture = CapturePlan::declare(&mut plan, 2);
        let storage = GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build();
        let mut graph = GraphBuilder::new();
        graph.add_callback("pair", || {
            Ok(TwoOutputs { swapped: false }.bind(declaration, &bindings, &bindings)?)
        });
        let recorder = ExecutionRecorder::new(1);
        let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
        let log = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            vec![capture.bind(&bindings).unwrap()],
        )
        .with_recording(recorder)
        .unwrap();
        graph.step(at(0)).unwrap();
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
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut ports = ReplayBindings::new();
    ports
        .add_output(
            "pair",
            0,
            bindings
                .configure_publisher(declaration.first_key(), |p| PortCapture::native(p, 1))
                .unwrap(),
        )
        .unwrap();
    ports
        .add_output(
            "pair",
            1,
            bindings
                .configure_publisher(declaration.second_key(), |p| PortCapture::native(p, 1))
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
        ports,
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
#[task_callback]
impl Forward {
    fn run<'storage>(
        &self,
        mut input: task::Input<'_, 'storage, Value>,
        output: &mut Publisher<'storage, task::ForwardedMessage<'storage, bool, Value>>,
    ) -> Result<(), LoanError> {
        if let Some(value) = input.pop() {
            output.publish(task::ForwardedMessage::new(true, value))?;
        }
        Ok(())
    }
}
struct Consume;
#[task_callback]
impl Consume {
    fn run(
        &self,
        input: RequiredInput<task::ForwardedMessage<'_, bool, Value>>,
        output: &mut Publisher<Value>,
    ) -> Result<(), LoanError> {
        output.publish(Value(
            input.forwarded.message.0 + if input.message { 10 } else { 100 },
        ))
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
                let a = Source::declare(&mut source).unwrap();
                let b = Forward::declare(&mut source, &mut forward).unwrap();
                let c = Consume::declare(&mut forward, &mut result).unwrap();
                source.reserve_retained(a.output_key(), 4).unwrap();
                let source_log = logged_source.then(|| CapturePlan::declare(&mut source, 2));
                let forward_log = logged_forward.then(|| CapturePlan::declare(&mut forward, 2));
                let result_log = CapturePlan::declare(&mut result, 2);
                let storage = GraphPlan::new((source, (forward, result)))
                    .allocate()
                    .unwrap();
                let source = storage.channels().0.build();
                let forward = storage.channels().1.0.build();
                let result = storage.channels().1.1.build();
                let mut graph = GraphBuilder::new();
                graph.add_callback("source", || {
                    Ok(Source { next: 1, mode: 0 }.bind(a, &source)?)
                });
                graph.add_callback("forward", || Ok(Forward.bind(b, &source, &forward)?));
                graph.add_callback("consume", || Ok(Consume.bind(c, &forward, &result)?));
                let recorder = ExecutionRecorder::new(8);
                let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
                let mut captures = vec![result_log.bind(&result).unwrap()];
                if let Some(capture) = source_log {
                    captures.push(capture.bind(&source).unwrap());
                }
                if let Some(capture) = forward_log {
                    captures.push(capture.bind(&forward).unwrap());
                }
                let log = LogSession::new(
                    logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
                    captures,
                )
                .with_recording(recorder)
                .unwrap();
                graph.step(at(0)).unwrap();
                graph.step(at(10)).unwrap();
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
            let a = Source::declare(&mut source).unwrap();
            let b = Forward::declare(&mut source, &mut forward).unwrap();
            let c = Consume::declare(&mut forward, &mut result).unwrap();
            let cache = exact_replay_executor::SourceCachePlan::declare(
                &mut source,
                log.source_capacity("source"),
            )
            .unwrap();
            let hydrate_b = ReplayInputPlan::declare(&mut source, b.input_key()).unwrap();
            let hydrate_c = ReplayInputPlan::declare(&mut forward, c.input_key()).unwrap();
            let storage = GraphPlan::new((source, (forward, result)))
                .allocate()
                .unwrap();
            let source = storage.channels().0.build();
            let forward = storage.channels().1.0.build();
            let result = storage.channels().1.1.build();
            let cache = cache.bind(&source).unwrap();
            let mut ports = ReplayBindings::new();
            let loader = cache.clone();
            ports
                .add_cache("source", move |h, b| loader.load(h, b))
                .unwrap();
            ports
                .add_input("forward", 0, hydrate_b.bind(&source).unwrap())
                .unwrap();
            ports
                .add_input(
                    "consume",
                    0,
                    hydrate_c
                        .bind_with_decoder(&forward, move |bytes| {
                            cache.decode_forwarded::<bool>(bytes)
                        })
                        .unwrap(),
                )
                .unwrap();
            ports
                .add_output(
                    "source",
                    0,
                    source
                        .configure_publisher(a.output_key(), |p| PortCapture::native(p, 4))
                        .unwrap(),
                )
                .unwrap();
            ports
                .add_output(
                    "forward",
                    0,
                    forward
                        .configure_publisher(b.output_key(), |p| PortCapture::native(p, 4))
                        .unwrap(),
                )
                .unwrap();
            ports
                .add_output(
                    "consume",
                    0,
                    result
                        .configure_publisher(c.output_key(), |p| PortCapture::native(p, 4))
                        .unwrap(),
                )
                .unwrap();
            let mut graph = GraphBuilder::new();
            graph.add_callback("source", || {
                Ok(Source { next: 1, mode: 0 }.bind(a, &source)?)
            });
            graph.add_callback("forward", || Ok(Forward.bind(b, &source, &forward)?));
            graph.add_callback("consume", || Ok(Consume.bind(c, &forward, &result)?));
            let report = ExactReplayExecutor::new(graph.build().unwrap(), log, ports)
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
