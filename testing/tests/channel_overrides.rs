use std::time::Duration;
use task::{
    CallbackSchedule, LoanError, Publisher, RequiredInput, TaskRegistration,
    automatic::{NamedPlan, Task},
    recording::{Direction, ExecutionRecorder},
};
use task_macros::task_callback;
use testing::UnitTestExecutorBuilder;

struct Scale;
#[task_callback]
impl Scale {
    fn run(
        &self,
        #[channel("sensor")] input: RequiredInput<u64>,
        #[trigger(false)] gain: RequiredInput<u64>,
        #[channel("results")] output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        output.publish(*input * *gain)
    }
}
#[test]
fn overrides_are_per_instance_and_preserve_annotation_and_argument_defaults() {
    let mut builder = UnitTestExecutorBuilder::new();
    builder
        .add_task("left", Scale, Duration::from_nanos(1))
        .input_channel("input", String::from("left_sensor"))
        .input_channel("gain", "left_gain")
        .output_channel("output", "left_results");
    builder.add_task("default", Scale, Duration::from_nanos(1));
    let mut left = builder.add_test_publisher::<u64>("left_sensor");
    let mut left_gain = builder.add_test_publisher::<u64>("left_gain");
    let left_result = builder.add_test_subscriber::<u64>("left_results");
    let mut default = builder.add_test_publisher::<u64>("sensor");
    let mut default_gain = builder.add_test_publisher::<u64>("gain");
    let default_result = builder.add_test_subscriber::<u64>("results");
    builder.run(|mut executor| {
        left.send(21);
        left_gain.send(2);
        executor.step();
        assert_eq!(
            left_result.messages(&mut executor, |_, m| assert_eq!(m.message, 42)),
            1
        );
        assert_eq!(
            default_result.messages(&mut executor, |_, _| panic!("wrong task instance")),
            0
        );
        default.send(7);
        default_gain.send(3);
        executor.step();
        assert_eq!(
            default_result.messages(&mut executor, |_, m| assert_eq!(m.message, 21)),
            1
        );
        assert_eq!(
            left_result.messages(&mut executor, |_, _| panic!("globally renamed channel")),
            0
        );
    });
}

fn unused_default() -> &'static str {
    panic!("overridden default evaluated")
}
struct Lazy;
#[task_callback]
impl Lazy {
    fn run(
        &self,
        #[channel(unused_default())] input: RequiredInput<u64>,
        output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        output.publish(*input)
    }
}
#[test]
fn shared_registration_resolves_once_and_keeps_callback_ordinals_independent() {
    let mut plan = NamedPlan::default();
    let extra_sub = plan.native::<u64>("chosen").unwrap().subscriber(1);
    let mut task = TaskRegistration::new(
        "lazy",
        Lazy,
        CallbackSchedule::default().with_execution_duration(Duration::from_nanos(1)),
    );
    task.input_channel("input", "unused")
        .input_channel("input", "chosen")
        .output_channel("output", "answer");
    let task = task.register(&mut plan).unwrap();
    assert!(plan.require("unused", false).is_err());
    assert!(plan.require("output", true).is_err());
    let sender = plan.native::<u64>("chosen").unwrap().publisher(1);
    let capture = plan.native::<u64>("answer").unwrap().subscriber(1);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut graph = storage.graph_builder();
    task.add_to_graph(&mut graph, &bindings);
    let recorder = ExecutionRecorder::new(1);
    let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
    let descriptor = recorder.descriptor().unwrap();
    let ports = &descriptor.callbacks[0].endpoints;
    assert_eq!(
        (
            ports[0].ordinal,
            ports[0].channel.as_str(),
            ports[0].direction
        ),
        (0, "chosen", Direction::Received)
    );
    assert_eq!(
        (
            ports[1].ordinal,
            ports[1].channel.as_str(),
            ports[1].direction
        ),
        (0, "answer", Direction::Published)
    );
    let mut sender = bindings
        .native::<u64>("chosen")
        .unwrap()
        .take_publisher(&sender)
        .unwrap();
    let extra = bindings
        .native::<u64>("chosen")
        .unwrap()
        .take_subscriber(&extra_sub)
        .unwrap();
    let capture = bindings
        .native::<u64>("answer")
        .unwrap()
        .take_subscriber(&capture)
        .unwrap();
    sender.publish(42).unwrap();
    sender.flush(task::time::FrameworkTime::from_nanoseconds(0));
    graph
        .step(task::time::FrameworkTime::from_nanoseconds(1))
        .unwrap();
    capture.update();
    assert_eq!(capture.input().value(), Some(&42));
    extra.update();
    assert_eq!(extra.input().value(), Some(&42));
    assert_eq!(recorder.drain()[0].inputs[0].ordinal, 0);
}

#[test]
fn invalid_port_overrides_fail_before_defaults_or_registration() {
    for (port, output) in [("typo", false), ("input", true), ("output", false)] {
        let mut overrides = task::ChannelOverrides::default();
        if output {
            overrides.output_channel(port, "chosen");
        } else {
            overrides.input_channel(port, "chosen");
        }
        let mut plan = NamedPlan::default();
        let error = Box::new(Lazy)
            .register_with(&mut plan, &overrides)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(port));
        assert!(plan.require("output", true).is_err());
    }
    let mut builder = UnitTestExecutorBuilder::new();
    builder
        .add_task("named", Scale, Duration::from_nanos(1))
        .output_channel("input", "wrong");
    let error = builder.try_allocate().err().unwrap().to_string();
    assert!(
        error.contains("named") && error.contains("input"),
        "{error}"
    );
}

#[test]
fn incompatible_channel_types_are_rejected_before_allocation() {
    struct Text;
    #[task_callback]
    impl Text {
        fn run(&self, input: RequiredInput<String>) {
            let _ = input;
        }
    }
    let mut builder = UnitTestExecutorBuilder::new();
    builder
        .add_task("numbers", Scale, Duration::from_nanos(1))
        .input_channel("input", "shared");
    builder
        .add_task("text", Text, Duration::from_nanos(1))
        .input_channel("input", "shared");
    let error = builder.try_allocate().err().unwrap().to_string();
    assert!(
        error.contains("text") && error.contains("shared") && error.contains("payload"),
        "{error}"
    );
}
