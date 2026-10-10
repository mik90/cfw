use logging::{AutomaticCapturePlan, CaptureOptions};
use task::{
    CallbackSchedule, InputSpan, LoanError, OutputSpan, Publisher,
    automatic::{NamedPlan, TaskRegistration},
    loggable::{DeserializeError, Loggable, SerializeError},
    time::FrameworkTime,
};
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
        #[capacity(4)] input: InputSpan<Value>,
        summary: &mut Publisher<Value>,
    ) -> Result<(), LoanError> {
        summary.publish(Value(input.inputs().map(|m| m.message.0).sum()))
    }
}

#[test]
fn discovers_custom_loggable_channels_deduplicates_and_applies_resolved_exclusions() {
    let mut plan = NamedPlan::default();
    let mut a = TaskRegistration::new("a", Source(10), CallbackSchedule::default());
    a.output_channel("output", "wire");
    let a = a.register(&mut plan).unwrap();
    let mut b = TaskRegistration::new("b", Source(20), CallbackSchedule::default());
    b.output_channel("output", "wire");
    let b = b.register(&mut plan).unwrap();
    let mut sum = TaskRegistration::new("sum", Sum, CallbackSchedule::default());
    sum.input_channel("input", "wire")
        .output_channel("summary", "totals");
    let sum = sum.register(&mut plan).unwrap();
    assert_eq!(
        plan.loggable_channels().collect::<Vec<_>>(),
        ["totals", "wire"]
    );
    let captures = AutomaticCapturePlan::declare(
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
    let mut builder = storage.graph_builder();
    a.add_to_graph(&mut builder, &bindings);
    b.add_to_graph(&mut builder, &bindings);
    sum.add_to_graph(&mut builder, &bindings);
    let mut graph = builder.build().unwrap();
    graph.step(FrameworkTime::from_nanoseconds(7)).unwrap();
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
    let mut plan = NamedPlan::default();
    let publisher = plan.publisher::<Value>("manual", 1).unwrap();
    plan.register_loggable_native::<Value>("manual").unwrap();
    plan.register_loggable_native::<Value>("manual").unwrap();
    assert!(plan.register_loggable_native::<Value>("unknown").is_err());
    plan.publisher::<Hidden>("opaque", 1).unwrap();
    assert!(plan.register_loggable_native::<Value>("opaque").is_err());
    assert!(AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(0)).is_err());
    let first = AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
    let second = AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
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
    let mut plan = NamedPlan::default();
    let task = TaskRegistration::new("hidden", Task, CallbackSchedule::default())
        .register(&mut plan)
        .unwrap();
    let captures = AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
    assert!(captures.channels().is_empty());
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    assert!(captures.bind(&bindings).unwrap().is_empty());
    let mut graph = storage.graph_builder();
    task.add_to_graph(&mut graph, &bindings);
    graph
        .build()
        .unwrap()
        .step(FrameworkTime::from_nanoseconds(0))
        .unwrap();
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
    let mut plan = NamedPlan::default();
    let source = TaskRegistration::new("serde", SerdeSource, CallbackSchedule::default())
        .register(&mut plan)
        .unwrap();
    let captures = AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
    assert_eq!(captures.channels(), ["output"]);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut captures = captures.bind(&bindings).unwrap();
    let mut graph = storage.graph_builder();
    source.add_to_graph(&mut graph, &bindings);
    graph
        .build()
        .unwrap()
        .step(FrameworkTime::from_nanoseconds(0))
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
        let mut plan = NamedPlan::default();
        let source = TaskRegistration::new("source", Source(5), CallbackSchedule::default())
            .register(&mut plan)
            .unwrap();
        let captures =
            AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(capacity)).unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut builder = storage.graph_builder();
        source.add_to_graph(&mut builder, &bindings);
        let recorder = ExecutionRecorder::new(1);
        let mut graph = recorder.attach(builder.build().unwrap()).unwrap();
        let bytes = Bytes::default();
        let session = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            captures.bind(&bindings).unwrap(),
        )
        .with_recording(recorder)
        .unwrap();
        graph.step(FrameworkTime::from_nanoseconds(0)).unwrap();
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
