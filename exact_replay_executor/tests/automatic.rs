use exact_replay_executor::{AutomaticExactReplayPlan, ExactReplayExecutor, ReplayLog};
use logging::{AutomaticCapturePlan, CaptureOptions, ExecutionRecorder, LogSession};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use task::{
    CallbackSchedule, InputSpan, LoanError, OutputSpan, Publisher,
    automatic::{NamedPlan, TaskRegistration},
    time::FrameworkTime,
};
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
        #[capacity(4)] first: InputSpan<Value>,
        #[capacity(2)] second: InputSpan<Value>,
        output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        output.publish(
            first.inputs().map(|m| m.message.0).sum::<u64>()
                + second.inputs().map(|m| m.message.0).sum::<u64>(),
        )
    }
}
fn source(plan: &mut NamedPlan, name: &str, value: u64) -> task::automatic::RegisteredTask {
    let mut registration = TaskRegistration::new(name, Source(value), CallbackSchedule::default());
    registration.output_channel("output", "shared");
    registration.register(plan).unwrap()
}
fn sum(plan: &mut NamedPlan) -> task::automatic::RegisteredTask {
    let mut registration = TaskRegistration::new("sum", Sum, CallbackSchedule::default());
    registration
        .input_channel("first", "shared")
        .input_channel("second", "shared");
    registration.register(plan).unwrap()
}
fn recorded(logged: bool) -> ReplayLog {
    let bytes = Bytes::default();
    {
        let mut plan = NamedPlan::default();
        let a = source(&mut plan, "a", 10);
        let b = source(&mut plan, "b", 20);
        let sum = sum(&mut plan);
        let options = if logged {
            CaptureOptions::new(8)
        } else {
            CaptureOptions::new(8).exclude("shared")
        };
        let capture = AutomaticCapturePlan::declare(&mut plan, &options).unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut graph = storage.graph_builder();
        a.add_to_graph(&mut graph, &bindings);
        b.add_to_graph(&mut graph, &bindings);
        sum.add_to_graph(&mut graph, &bindings);
        let recorder = ExecutionRecorder::new(6);
        let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
        let session = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            capture.bind(&bindings).unwrap(),
        )
        .with_recording(recorder)
        .unwrap();
        graph.step(FrameworkTime::from_nanoseconds(0)).unwrap();
        graph.step(FrameworkTime::from_nanoseconds(10)).unwrap();
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
        let mut plan = NamedPlan::default();
        let b = source(&mut plan, "b", 20);
        let sum = sum(&mut plan);
        let a = source(&mut plan, "a", 10);
        let cache =
            AutomaticExactReplayPlan::source_cache::<Value>(&mut plan, &log, "shared").unwrap();
        let replay = AutomaticExactReplayPlan::declare(&mut plan, &log).unwrap();
        assert!(AutomaticExactReplayPlan::declare(&mut plan, &log).is_err());
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut replay = replay.bind(&bindings).unwrap();
        let cache = cache.bind(&bindings, &mut replay).unwrap();
        let mut graph = storage.graph_builder();
        sum.add_to_graph(&mut graph, &bindings);
        b.add_to_graph(&mut graph, &bindings);
        a.add_to_graph(&mut graph, &bindings);
        let report = ExactReplayExecutor::new(graph.build().unwrap(), log, replay)
            .unwrap()
            .run()
            .unwrap();
        assert!(report.is_exact(), "{report:?}");
        assert_eq!(report.consumed_executions(), 6);
        let forwarded: task::ForwardedMessage<'_, bool, Value> = cache.decode_forwarded(&serde_json::to_vec(&serde_json::json!({
            "message": true,
            "forwarded_message_header": { "published_at": { "nanoseconds": 0 }, "publisher_index": 0, "batch_index": 0 }
        })).unwrap()).unwrap();
        assert_eq!(forwarded.forwarded.message.0, 11);
    }
}

#[test]
fn mismatched_layout_is_rejected_before_hydration_publishers_are_declared() {
    let log = recorded(true);
    let mut plan = NamedPlan::default();
    let _a = source(&mut plan, "a", 10);
    let _b = source(&mut plan, "b", 20);
    let mut registration = TaskRegistration::new("sum", Sum, CallbackSchedule::default());
    registration
        .input_channel("first", "shared")
        .input_channel("second", "wrong");
    let _sum = registration.register(&mut plan).unwrap();
    assert!(AutomaticExactReplayPlan::declare(&mut plan, &log).is_err());
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
fn explicit_custom_ports_can_be_combined_with_automatic_bindings() {
    use exact_replay_executor::{ExplicitReplayPort, ReplayInputPlan};
    let log = recorded(true);
    let mut plan = NamedPlan::default();
    let a = source(&mut plan, "a", 10);
    let b = source(&mut plan, "b", 20);
    let sum = sum(&mut plan);
    assert!(plan.native_input_key::<u64>("sum", 1).is_err());
    let input_key = plan.native_input_key::<Value>("sum", 1).unwrap();
    let output_key = plan.native_output_key::<Value>("a", 0).unwrap();
    assert!(
        AutomaticExactReplayPlan::declare_with_explicit(
            &mut plan,
            &log,
            &std::collections::BTreeSet::from([ExplicitReplayPort::input("missing", 0)])
        )
        .is_err()
    );
    let explicit = std::collections::BTreeSet::from([
        ExplicitReplayPort::input("sum", 1),
        ExplicitReplayPort::output("a", 0),
    ]);
    let manual =
        ReplayInputPlan::declare(plan.native::<Value>("shared").unwrap(), &input_key).unwrap();
    let automatic =
        AutomaticExactReplayPlan::declare_with_explicit(&mut plan, &log, &explicit).unwrap();
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
                    logging::PortCapture::native(publisher, 2)
                })
                .unwrap(),
        )
        .unwrap();
    let mut graph = storage.graph_builder();
    a.add_to_graph(&mut graph, &bindings);
    b.add_to_graph(&mut graph, &bindings);
    sum.add_to_graph(&mut graph, &bindings);
    assert!(
        ExactReplayExecutor::new(graph.build().unwrap(), log, replay)
            .unwrap()
            .run()
            .unwrap()
            .is_exact()
    );
}

struct Contextual(u64);
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
        let mut plan = NamedPlan::default();
        let source = TaskRegistration::new("source", ContextualSource, CallbackSchedule::default())
            .register(&mut plan)
            .unwrap();
        assert!(plan.replayable_channels().next().is_none());
        let capture = AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut graph = storage.graph_builder();
        source.add_to_graph(&mut graph, &bindings);
        let recorder = ExecutionRecorder::new(1);
        let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
        let session = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            capture.bind(&bindings).unwrap(),
        )
        .with_recording(recorder)
        .unwrap();
        graph.step(FrameworkTime::from_nanoseconds(0)).unwrap();
        session.finish().unwrap();
    }
    let log = ReplayLog::from_reader(
        &logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap(),
    )
    .unwrap();
    let mut plan = NamedPlan::default();
    let source = TaskRegistration::new("source", ContextualSource, CallbackSchedule::default())
        .register(&mut plan)
        .unwrap();
    let replay = AutomaticExactReplayPlan::declare(&mut plan, &log).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let replay = replay.bind(&bindings).unwrap();
    let mut graph = storage.graph_builder();
    source.add_to_graph(&mut graph, &bindings);
    assert!(
        ExactReplayExecutor::new(graph.build().unwrap(), log, replay)
            .unwrap()
            .run()
            .unwrap()
            .is_exact()
    );
}
