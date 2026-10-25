use exact_replay_executor::{ExactReplayExecutor, ExactReplayPlan, ReplayLog};
use logging::{CaptureOptions, ExecutionRecorder, GraphCapturePlan, LogSession};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use task::loggable::Loggable;
use task::{InputSpan, LoanError, OutputSpan, Publisher, time::FrameworkTime};
use task_graph::{CallbackSchedule, GraphPlan, TaskRegistration};
use task_macros::task_callback;
type SourcePtr<'a, T> = task::ArenaReaderPtr<'a, task::message::Message<T>>;

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
#[derive(serde::Serialize, serde::Deserialize)]
struct Value(u64);
struct Source(u64);
#[task_callback]
impl Source {
    fn run(&mut self, #[capacity(2)] output: OutputSpan<Value>) -> Result<(), LoanError> {
        let a = output.loan(Value(self.0))?;
        let b = output.loan(Value(self.0 + 1))?;
        b.send();
        a.send();
        self.0 += 2;
        Ok(())
    }
}
struct Sum;
#[task_callback]
impl Sum {
    fn run(
        &self,
        #[trigger(true)]
        #[capacity(4)]
        first: InputSpan<Value>,
        #[trigger(true)]
        #[capacity(2)]
        second: InputSpan<Value>,
        output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        output.publish(
            first.inputs().map(|m| m.message.0).sum::<u64>()
                + second.inputs().map(|m| m.message.0).sum::<u64>(),
        )
    }
}
fn source(plan: &mut GraphPlan, name: &str, value: u64) {
    let mut registration =
        TaskRegistration::new(name, Source(value), CallbackSchedule::input_triggered());
    registration.output_channel("output", "shared");
    registration.register(plan).unwrap()
}
fn sum(plan: &mut GraphPlan) {
    let mut registration = TaskRegistration::new("sum", Sum, CallbackSchedule::input_triggered());
    registration
        .input_channel("first", "shared")
        .input_channel("second", "shared");
    registration.register(plan).unwrap()
}
fn recorded(logged: bool) -> ReplayLog {
    let bytes = Bytes::default();
    {
        let mut plan = GraphPlan::default();
        source(&mut plan, "a", 10);
        source(&mut plan, "b", 20);
        sum(&mut plan);
        let options = if logged {
            CaptureOptions::new(8)
        } else {
            CaptureOptions::new(8).exclude("shared")
        };
        let capture = GraphCapturePlan::declare(&mut plan, &options).unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let recorder = ExecutionRecorder::new(6);
        let mut graph = storage
            .build_with(&bindings)
            .unwrap()
            .with_recording(&recorder)
            .unwrap();
        graph.configure_schedules(|_, schedule| {
            *schedule = CallbackSchedule {
                run_on_start: true,
                ..CallbackSchedule::periodic(std::time::Duration::from_nanos(10))
                    .with_execution_duration(std::time::Duration::ZERO)
            };
        });
        let mut graph = testing::UnitTestExecutor::new_with_graph(graph, Default::default());
        let session = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            capture.bind(&bindings).unwrap(),
        )
        .with_recording(recorder)
        .unwrap();
        graph
            .run_until(FrameworkTime::from_nanoseconds(11), 32)
            .unwrap();
        session.finish().unwrap();
    }
    ReplayLog::from_reader(
        &logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn automatic_exact_bindings_restore_distinct_windows_and_reordered_publishers() {
    for logged in [false, true] {
        let log = recorded(logged);
        let mut plan = GraphPlan::default();
        sum(&mut plan);
        source(&mut plan, "b", 20);
        source(&mut plan, "a", 10);
        let cache = ExactReplayPlan::source_cache::<Value>(&mut plan, &log, "shared").unwrap();
        let replay = ExactReplayPlan::declare(&mut plan, &log).unwrap();
        assert!(ExactReplayPlan::declare(&mut plan, &log).is_err());
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut replay = replay.bind(&bindings).unwrap();
        let cache = cache.bind(&bindings, &mut replay).unwrap();
        let report = ExactReplayExecutor::new(storage.build_with(&bindings).unwrap(), log, replay)
            .unwrap()
            .run()
            .unwrap();
        assert!(report.is_exact(), "{report:?}");
        assert_eq!(report.consumed_executions(), 6);
        let forwarded = task::Forwarded::<bool,SourcePtr<'_,Value>>::deserialize_with_ctx(&serde_json::to_vec(&serde_json::json!([
            true, [["source", { "published_at": { "nanoseconds": 0 }, "publisher_index": 0, "batch_index": 0 }]]
        ])).unwrap(), &cache).unwrap();
        assert_eq!(forwarded.forwarded_messages.message.0, 11);
    }
}

#[test]
fn mismatched_layout_is_rejected_before_hydration_publishers_are_declared() {
    let log = recorded(true);
    let mut plan = GraphPlan::default();
    source(&mut plan, "a", 10);
    source(&mut plan, "b", 20);
    let mut registration = TaskRegistration::new("sum", Sum, CallbackSchedule::input_triggered());
    registration
        .input_channel("first", "shared")
        .input_channel("second", "wrong");
    registration.register(&mut plan).unwrap();
    assert!(ExactReplayPlan::declare(&mut plan, &log).is_err());
    let key = plan.native::<Value>("shared").unwrap().publisher(1);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    assert_eq!(
        bindings
            .native::<Value>("shared")
            .unwrap()
            .take_publisher(&key)
            .unwrap()
            .publisher_index(),
        2
    );
}

#[test]
fn explicit_custom_endpoints_can_be_combined_with_automatic_bindings() {
    use exact_replay_executor::{ExplicitReplayEndpoint, ReplayInputPlan};
    let log = recorded(true);
    let mut plan = GraphPlan::default();
    source(&mut plan, "a", 10);
    source(&mut plan, "b", 20);
    sum(&mut plan);
    assert!(plan.native_input_key::<u64>("sum", 1).is_err());
    let input_key = plan.native_input_key::<Value>("sum", 1).unwrap();
    let output_key = plan.native_output_key::<Value>("a", 0).unwrap();
    assert!(
        ExactReplayPlan::declare_with_explicit(
            &mut plan,
            &log,
            &std::collections::BTreeSet::from([ExplicitReplayEndpoint::input("missing", 0)])
        )
        .is_err()
    );
    let explicit = std::collections::BTreeSet::from([
        ExplicitReplayEndpoint::input("sum", 1),
        ExplicitReplayEndpoint::output("a", 0),
    ]);
    let manual =
        ReplayInputPlan::declare(plan.native::<Value>("shared").unwrap(), &input_key).unwrap();
    let automatic = ExactReplayPlan::declare_with_explicit(&mut plan, &log, &explicit).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut replay = automatic.bind(&bindings).unwrap();
    replay
        .add_input(
            "sum",
            1,
            manual
                .bind_with_decoder(bindings.native::<Value>("shared").unwrap(), |bytes| {
                    Ok(serde_json::from_slice(bytes)?)
                })
                .unwrap(),
        )
        .unwrap();
    replay
        .add_output(
            "a",
            0,
            bindings
                .native::<Value>("shared")
                .unwrap()
                .configure_publisher(&output_key, |publisher| {
                    logging::EndpointCapture::native(publisher, 2)
                })
                .unwrap(),
        )
        .unwrap();
    assert!(
        ExactReplayExecutor::new(storage.build_with(&bindings).unwrap(), log, replay)
            .unwrap()
            .run()
            .unwrap()
            .is_exact()
    );
}

struct Contextual(u64);

#[derive(task_macros::ForwardedSources)]
struct PairSources<'s> {
    left: task::ArenaReaderPtr<'s, task::message::Message<u64>>,
    right: task::ArenaReaderPtr<'s, task::message::Message<u32>>,
}
struct PairProducer;
#[task_callback]
impl PairProducer {
    fn run(&self, left: &mut Publisher<u64>, right: &mut Publisher<u32>) -> Result<(), LoanError> {
        left.publish(7)?;
        right.publish(5)
    }
}
struct PairForward;
#[task_callback]
impl PairForward {
    fn run<'s>(
        &self,
        #[trigger(true)] mut left: task::RequiredInput<'_, 's, u64, task::Forwardable>,
        #[trigger(true)] mut right: task::RequiredInput<'_, 's, u32, task::Forwardable>,
        #[forwards(left = left, right = right)] pair: task::OutputUninit<
            '_,
            's,
            task::Forwarded<bool, PairSources<'s>>,
        >,
    ) {
        pair.write(task::Forwarded::new(
            true,
            PairSources {
                left: left.forward().unwrap(),
                right: right.forward().unwrap(),
            },
        ))
        .send();
    }
}
struct PairSink;
#[task_callback]
impl PairSink {
    fn run<'s>(
        &self,
        #[trigger(true)] pair: task::RequiredInput<'_, 's, task::Forwarded<bool, PairSources<'s>>>,
        result: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        result.publish(
            pair.forwarded_messages.left.message + u64::from(pair.forwarded_messages.right.message),
        )
    }
}
fn pair_tasks(plan: &mut GraphPlan, reverse: bool) {
    let mut tasks = vec![
        TaskRegistration::new(
            "producer",
            PairProducer,
            CallbackSchedule::input_triggered(),
        ),
        TaskRegistration::new("forward", PairForward, CallbackSchedule::input_triggered()),
        TaskRegistration::new("sink", PairSink, CallbackSchedule::input_triggered()),
    ];
    if reverse {
        tasks.reverse();
    }
    for task in tasks {
        task.register(plan).unwrap();
    }
}
#[test]
fn heterogeneous_bundles_replay_logged_and_reproduced_sources() {
    use exact_replay_executor::{BundleReplayInputPlan, ExplicitReplayEndpoint};
    use task::loggable::Loggable;
    for logged in [false, true] {
        let bytes = Bytes::default();
        {
            let mut plan = GraphPlan::default();
            pair_tasks(&mut plan, false);
            let options = if logged {
                CaptureOptions::new(16)
            } else {
                CaptureOptions::new(16).exclude("left").exclude("right")
            };
            let capture = GraphCapturePlan::declare(&mut plan, &options).unwrap();
            let storage = plan.allocate().unwrap();
            let bindings = storage.bind().unwrap();
            let recorder = ExecutionRecorder::new(12);
            let mut graph = storage
                .build_with(&bindings)
                .unwrap()
                .with_recording(&recorder)
                .unwrap();
            graph.configure_schedules(|_, schedule| {
                *schedule = CallbackSchedule {
                    run_on_start: true,
                    ..CallbackSchedule::periodic(std::time::Duration::from_nanos(1))
                        .with_execution_duration(std::time::Duration::ZERO)
                };
            });
            let mut graph = testing::UnitTestExecutor::new_with_graph(graph, Default::default());
            let session = LogSession::new(
                logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
                capture.bind(&bindings).unwrap(),
            )
            .with_recording(recorder)
            .unwrap();
            graph
                .run_until(FrameworkTime::from_nanoseconds(4), 32)
                .unwrap();
            session.finish().unwrap();
        }
        let log = ReplayLog::from_reader(
            &logging::log_file_json::JsonLogFileReader::from_reader(
                bytes.0.lock().unwrap().as_slice(),
            )
            .unwrap(),
        )
        .unwrap();
        let mut plan = GraphPlan::default();
        pair_tasks(&mut plan, true);
        let left = ExactReplayPlan::source_cache::<u64>(&mut plan, &log, "left").unwrap();
        let right = ExactReplayPlan::source_cache::<u32>(&mut plan, &log, "right").unwrap();
        let key = plan
            .bundle_input_key::<bool, PairSources<'static>>("sink", 0)
            .unwrap();
        let manual = BundleReplayInputPlan::<bool, PairSources<'static>>::declare(
            plan.bundle::<bool, PairSources<'static>>("pair").unwrap(),
            &key,
        )
        .unwrap();
        let automatic = ExactReplayPlan::declare_with_explicit(
            &mut plan,
            &log,
            &std::collections::BTreeSet::from([ExplicitReplayEndpoint::input("sink", 0)]),
        )
        .unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut replay = automatic.bind(&bindings).unwrap();
        let left = left.bind(&bindings, &mut replay).unwrap();
        let right = right.bind(&bindings, &mut replay).unwrap();
        replay
            .add_input(
                "sink",
                0,
                manual
                    .bind_with_decoder(
                        bindings
                            .bundle::<bool, PairSources<'static>>("pair")
                            .unwrap(),
                        move |bytes| {
                            task::Forwarded::<bool, PairSources<'_>>::deserialize_with_ctx(
                                bytes,
                                (&left, &right),
                            )
                        },
                    )
                    .unwrap(),
            )
            .unwrap();
        assert!(
            ExactReplayExecutor::new(storage.build_with(&bindings).unwrap(), log, replay)
                .unwrap()
                .run()
                .unwrap()
                .is_exact()
        );
    }
}
struct ForwardSource(u64);
#[task_callback]
impl ForwardSource {
    fn run(&mut self, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(self.0)?;
        self.0 += 1;
        Ok(())
    }
}
struct ForwardTask;
#[task_callback]
impl ForwardTask {
    fn run<'storage>(
        &self,
        #[trigger(true)] mut input: task::Input<'_, 'storage, u64, task::Forwardable>,
        #[forwards(source = input)] output: &mut Publisher<
            'storage,
            task::Forwarded<bool, SourcePtr<'storage, u64>>,
        >,
    ) -> Result<(), LoanError> {
        for source in input.drain() {
            output.publish(task::Forwarded::new(true, source))?;
        }
        Ok(())
    }
}
struct ForwardSink;
#[task_callback]
impl ForwardSink {
    fn run<'storage>(
        &self,
        #[trigger(true)] mut input: task::Input<
            '_,
            'storage,
            task::Forwarded<bool, SourcePtr<'storage, u64>>,
        >,
        output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        for forwarded in input.drain() {
            output.publish(forwarded.message.forwarded_messages.message + 1)?;
        }
        Ok(())
    }
}
fn forwarding_tasks(plan: &mut GraphPlan, reverse: bool) {
    let mut source = TaskRegistration::new(
        "source",
        ForwardSource(10),
        CallbackSchedule::input_triggered(),
    );
    source.output_channel("output", "source-values");
    let mut forward =
        TaskRegistration::new("forward", ForwardTask, CallbackSchedule::input_triggered());
    forward
        .input_channel("input", "source-values")
        .output_channel("output", "forwarded");
    let mut sink = TaskRegistration::new("sink", ForwardSink, CallbackSchedule::input_triggered());
    sink.input_channel("input", "forwarded")
        .output_channel("output", "result");
    let tasks = if reverse {
        vec![sink, forward, source]
    } else {
        vec![source, forward, sink]
    };
    for task in tasks {
        task.register(plan).unwrap();
    }
}
#[test]
fn borrowed_forwarding_replays_logged_and_reproduced_source_caches() {
    use exact_replay_executor::{BundleReplayInputPlan, ExplicitReplayEndpoint};
    for logged in [false, true] {
        let bytes = Bytes::default();
        {
            let mut plan = GraphPlan::default();
            forwarding_tasks(&mut plan, false);
            let options = if logged {
                CaptureOptions::new(8)
            } else {
                CaptureOptions::new(8).exclude("source-values")
            };
            let capture = GraphCapturePlan::declare(&mut plan, &options).unwrap();
            let storage = plan.allocate().unwrap();
            let bindings = storage.bind().unwrap();
            let recorder = ExecutionRecorder::new(9);
            let mut graph = storage
                .build_with(&bindings)
                .unwrap()
                .with_recording(&recorder)
                .unwrap();
            graph.configure_schedules(|_, schedule| {
                *schedule = CallbackSchedule {
                    run_on_start: true,
                    ..CallbackSchedule::periodic(std::time::Duration::from_nanos(1))
                        .with_execution_duration(std::time::Duration::ZERO)
                };
            });
            let mut graph = testing::UnitTestExecutor::new_with_graph(graph, Default::default());
            let log = LogSession::new(
                logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
                capture.bind(&bindings).unwrap(),
            )
            .with_recording(recorder)
            .unwrap();
            graph
                .run_until(FrameworkTime::from_nanoseconds(3), 32)
                .unwrap();
            log.finish().unwrap();
        }
        let log = ReplayLog::from_reader(
            &logging::log_file_json::JsonLogFileReader::from_reader(
                bytes.0.lock().unwrap().as_slice(),
            )
            .unwrap(),
        )
        .unwrap();
        let mut plan = GraphPlan::default();
        forwarding_tasks(&mut plan, true);
        let cache = ExactReplayPlan::source_cache::<u64>(&mut plan, &log, "source-values").unwrap();
        let input = plan
            .bundle_input_key::<bool, SourcePtr<'static, u64>>("sink", 0)
            .unwrap();
        let manual = BundleReplayInputPlan::<bool, SourcePtr<'static, u64>>::declare(
            plan.bundle::<bool, SourcePtr<'static, u64>>("forwarded")
                .unwrap(),
            &input,
        )
        .unwrap();
        let automatic = ExactReplayPlan::declare_with_explicit(
            &mut plan,
            &log,
            &std::collections::BTreeSet::from([ExplicitReplayEndpoint::input("sink", 0)]),
        )
        .unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut replay = automatic.bind(&bindings).unwrap();
        let cache = cache.bind(&bindings, &mut replay).unwrap();
        replay
            .add_input(
                "sink",
                0,
                manual
                    .bind_with_decoder(
                        bindings
                            .bundle::<bool, SourcePtr<'static, u64>>("forwarded")
                            .unwrap(),
                        move |bytes| {
                            task::Forwarded::<bool, SourcePtr<'_, u64>>::deserialize_with_ctx(
                                bytes, &cache,
                            )
                        },
                    )
                    .unwrap(),
            )
            .unwrap();
        assert!(
            ExactReplayExecutor::new(storage.build_with(&bindings).unwrap(), log, replay)
                .unwrap()
                .run()
                .unwrap()
                .is_exact()
        );
    }
}
impl task::loggable::Loggable for Contextual {
    type Context<'a> = &'a u64;
    fn serialize(&self, writer: &mut dyn Write) -> Result<(), task::loggable::SerializeError> {
        write!(writer, "{}", self.0)?;
        Ok(())
    }
    fn deserialize_with_ctx<'a>(
        _: &[u8],
        value: &'a u64,
    ) -> Result<Self, task::loggable::DeserializeError>
    where
        Self: 'a,
    {
        Ok(Self(*value))
    }
}
struct ContextualSource;
#[task_callback]
impl ContextualSource {
    fn run(&self, output: &mut Publisher<Contextual>) -> Result<(), LoanError> {
        output.publish(Contextual(42))
    }
}
#[test]
fn contextual_output_capture_does_not_require_a_context_free_decoder() {
    let bytes = Bytes::default();
    {
        let mut plan = GraphPlan::default();
        TaskRegistration::new(
            "source",
            ContextualSource,
            CallbackSchedule::input_triggered(),
        )
        .register(&mut plan)
        .unwrap();
        assert!(plan.replayable_channels().next().is_none());
        let capture = GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
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
        let session = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            capture.bind(&bindings).unwrap(),
        )
        .with_recording(recorder)
        .unwrap();
        graph.run_until_idle(8).unwrap();
        session.finish().unwrap();
    }
    let log = ReplayLog::from_reader(
        &logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap(),
    )
    .unwrap();
    let mut plan = GraphPlan::default();
    TaskRegistration::new(
        "source",
        ContextualSource,
        CallbackSchedule::input_triggered(),
    )
    .register(&mut plan)
    .unwrap();
    let replay = ExactReplayPlan::declare(&mut plan, &log).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let replay = replay.bind(&bindings).unwrap();
    assert!(
        ExactReplayExecutor::new(storage.build_with(&bindings).unwrap(), log, replay)
            .unwrap()
            .run()
            .unwrap()
            .is_exact()
    );
}
