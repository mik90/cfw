task_cxx_macros::define_cxx_bridge! {
    namespace: "cfw::fizz_buzz",
    headers: ["cxx_fizz_buzz/src/fizz_buzz.hpp"],
    payloads: {
        Vec3: {
            namespace: "cfw::messages",
            header: "cxx_fizz_buzz_messages/src/lib.rs.h",
            rust_path: ::cxx_fizz_buzz_messages::Vec3,
        },
    },
}

use cxx_fizz_buzz_messages::Vec3;

pub const INTEGER_CHANNEL: &str = "integer";
pub const FIZZ_BUZZ_STRING_CHANNEL: &str = "fizz_buzz_string";
pub const OPTIONAL_PROBE_IN_CHANNEL: &str = "optional_probe_in";
pub const OPTIONAL_PROBE_OUT_CHANNEL: &str = "optional_probe_out";
pub const NORM_IN_CHANNEL: &str = "norm_in";
pub const NORM_OUT_CHANNEL: &str = "norm_out";
pub const SPAN_IN_CHANNEL: &str = "span_in";
pub const SPAN_OUT_CHANNEL: &str = "span_out";
pub const SPAN_LABEL_CHANNEL: &str = "span_label";

task_cxx_macros::define_cxx_task! {
    bridge: ffi,
    name: FizzBuzzCalculator,
    cxx_type: FizzBuzzTask,
    factory: make_fizz_buzz_task,
    subscribers: {
        integer: RequiredInput<u64> { channel: "integer", trigger: true },
    },
    publishers: {
        fizz_buzz_string: Output<String> { channel: "fizz_buzz_string" },
    },
}

task_cxx_macros::define_cxx_task! {
    bridge: ffi,
    name: OptionalProbe,
    cxx_type: OptionalProbeTask,
    factory: make_optional_probe_task,
    subscribers: {
        value: InputSpan<u64> { channel: "optional_probe_in", capacity: 4, trigger: true },
    },
    publishers: {
        report: Output<String> { channel: "optional_probe_out" },
    },
}

task_cxx_macros::define_cxx_task! {
    bridge: ffi,
    name: SpanProbe,
    cxx_type: SpanProbeTask,
    factory: make_span_probe_task,
    subscribers: {
        count: RequiredInput<u64> { channel: "span_in", trigger: true },
    },
    publishers: {
        numbers: OutputSpan<u64> { channel: "span_out", capacity: 4 },
        label: OutputUninit<String> { channel: "span_label" },
    },
}

task_cxx_macros::define_cxx_task! {
    bridge: ffi,
    name: NormCalculator,
    cxx_type: NormTask,
    factory: make_norm_task,
    fallible: true,
    subscribers: {
        value: RequiredInput<Vec3> { channel: "norm_in", trigger: true },
    },
    publishers: {
        length: Output<f64> { channel: "norm_out" },
    },
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::time::Duration;

    use cxx_fizz_buzz_messages::Vec3;
    use task::time::FrameworkTime;
    use task_graph::CallbackSchedule;
    use test_tasks::{IncrementingIntegerPublisher, StringCollector, fizz_buzz};
    use testing::UnitTestExecutorBuilder;

    use super::{
        FIZZ_BUZZ_STRING_CHANNEL, FizzBuzzCalculator, INTEGER_CHANNEL, NORM_IN_CHANNEL,
        NORM_OUT_CHANNEL, NormCalculator, OPTIONAL_PROBE_IN_CHANNEL, OPTIONAL_PROBE_OUT_CHANNEL,
        OptionalProbe, SPAN_IN_CHANNEL, SPAN_LABEL_CHANNEL, SPAN_OUT_CHANNEL, SpanProbe,
    };

    #[test]
    fn cxx_calculator_matches_rust_reference() {
        let collector = StringCollector::default();
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task(
            "calculator",
            FizzBuzzCalculator::new(),
            Duration::from_millis(5),
        );
        builder.add_task("collector", collector.clone(), Duration::from_millis(2));
        let mut input = builder.add_test_publisher::<u64>(INTEGER_CHANNEL);
        builder.run(|mut executor| {
            for value in 0..16 {
                input.send(value);
                executor.step();
                executor.step();
            }
        });
        assert_eq!(
            collector.stored_strings(),
            (0..16).map(fizz_buzz).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cxx_calculator_wires_declared_channels() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task(
            "calculator",
            FizzBuzzCalculator::new(),
            Duration::from_millis(5),
        );
        let _input = builder.add_test_publisher::<u64>(INTEGER_CHANNEL);
        let _output = builder.add_test_subscriber::<String>(FIZZ_BUZZ_STRING_CHANNEL);
        builder.run(|_| {});
    }

    #[test]
    fn cxx_optional_input_queue_and_header_semantics() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("probe", OptionalProbe::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<u64>(OPTIONAL_PROBE_IN_CHANNEL);
        let output = builder.add_test_subscriber::<String>(OPTIONAL_PROBE_OUT_CHANNEL);
        let mut reports = Vec::new();
        builder.run(|mut executor| {
            input.send_at(FrameworkTime::from_millis(3), 7);
            input.send_at(FrameworkTime::from_millis(3), 9);
            for _ in 0..3 {
                executor.step();
                output.messages(&mut executor, |_, message| {
                    reports.push(message.message.clone());
                });
            }
        });
        assert_eq!(reports.first().map(String::as_str), Some("7:2:7:3000000"));
        assert!(reports.len() <= 2);
        if let Some(second) = reports.get(1) {
            assert_eq!(second, "9:1:9:3000000");
        }
    }

    #[test]
    fn cxx_span_and_uninit_outputs() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("span", SpanProbe::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<u64>(SPAN_IN_CHANNEL);
        let numbers = builder.add_test_subscriber::<u64>(SPAN_OUT_CHANNEL);
        let labels = builder.add_test_subscriber::<String>(SPAN_LABEL_CHANNEL);
        let mut collected_numbers = Vec::new();
        let mut collected_labels = Vec::new();
        builder.run(|mut executor| {
            input.send(6);
            executor.step();
            numbers.messages(&mut executor, |_, message| {
                collected_numbers.push(message.message);
            });
            labels.messages(&mut executor, |_, message| {
                collected_labels.push(message.message.clone());
            });
        });
        assert_eq!(collected_numbers, vec![1, 0, 2, 3]);
        assert_eq!(collected_labels, vec!["span:4".to_string()]);
    }

    #[test]
    fn cxx_uninit_output_unsent_is_discarded() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("span", SpanProbe::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<u64>(SPAN_IN_CHANNEL);
        let labels = builder.add_test_subscriber::<String>(SPAN_LABEL_CHANNEL);
        let mut collected = Vec::new();
        builder.run(|mut executor| {
            input.send(0);
            executor.step();
            labels.messages(&mut executor, |_, message| {
                collected.push(message.message.clone());
            });
            input.send(2);
            executor.step();
            labels.messages(&mut executor, |_, message| {
                collected.push(message.message.clone());
            });
        });
        assert_eq!(collected, vec!["span:2".to_string()]);
    }

    #[test]
    fn cxx_shared_struct_payload_runs_through_cxx() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("norm", NormCalculator::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<Vec3>(NORM_IN_CHANNEL);
        let output = builder.add_test_subscriber::<f64>(NORM_OUT_CHANNEL);
        let mut lengths = Vec::new();
        builder.run(|mut executor| {
            input.send(Vec3 {
                x: 3.0,
                y: 4.0,
                z: 12.0,
            });
            executor.step();
            output.messages(&mut executor, |_, message| lengths.push(message.message));
        });
        assert_eq!(lengths, vec![13.0]);
    }

    #[test]
    fn cxx_fallible_run_surfaces_exceptions() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("norm", NormCalculator::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<Vec3>(NORM_IN_CHANNEL);
        let error = builder.run(|mut executor| {
            input.send(Vec3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            });
            executor.try_step().unwrap_err()
        });
        match error {
            testing::StepError::Callback {
                failure: testing::BatchFailure::Panic,
                ..
            } => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn cxx_calculator_runs_under_live_executor() {
        let collector = StringCollector::default();
        let mut app = task_graph::ApplicationBuilder::new();
        app.add_task(
            "producer",
            IncrementingIntegerPublisher::default(),
            CallbackSchedule::periodic(Duration::from_millis(10))
                .with_execution_duration(Duration::from_millis(1)),
        );
        app.add_task(
            "calculator",
            FizzBuzzCalculator::new(),
            CallbackSchedule::input_triggered().with_execution_duration(Duration::from_millis(1)),
        );
        app.add_task(
            "collector",
            collector.clone(),
            CallbackSchedule::input_triggered().with_execution_duration(Duration::from_millis(1)),
        );
        let storage = app.allocate().unwrap();
        let (graph, bindings) = storage.bind_and_build().unwrap();
        live_executor::LiveExecutor::new(2, graph)
            .unwrap()
            .run_with(|stop| {
                let start = std::time::Instant::now();
                while collector.len() < 4 {
                    assert!(
                        start.elapsed() < Duration::from_secs(5),
                        "timed out waiting for C++ callback output"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                stop.request_stop();
            })
            .unwrap();
        drop(bindings);

        let collected = collector.stored_strings();
        assert!(!collected.is_empty());
        for (index, value) in collected.iter().enumerate() {
            assert_eq!(*value, fizz_buzz(index as u64));
        }
    }
}
