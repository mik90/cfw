//! Hand-written C++ task bridge (M1).
//!
//! The literal `#[cxx::bridge]` module must stay in source because cxx codegen
//! scans files, so its `extern "Rust"` declarations follow a naming convention
//! derived from the endpoint names. `define_cxx_task!` generates the view types,
//! the task struct, and the `task_graph::Task` adapter that matches them.

pub const INTEGER_CHANNEL: &str = "integer";
pub const FIZZ_BUZZ_STRING_CHANNEL: &str = "fizz_buzz_string";

#[cxx::bridge(namespace = "cfw::fizz_buzz")]
mod ffi {
    #[namespace = "cfw"]
    unsafe extern "C++" {
        include!("task_cxx/src/lib.rs.h");

        type Context = ::task_cxx::ffi::Context;
    }

    unsafe extern "C++" {
        include!("cxx_fizz_buzz/src/fizz_buzz.hpp");

        type FizzBuzzTask;

        fn run(
            self: Pin<&mut FizzBuzzTask>,
            inputs: &mut FizzBuzzCalculatorCxxInputs,
            outputs: &mut FizzBuzzCalculatorCxxOutputs,
            context: &Context,
        );
        fn make_fizz_buzz_task() -> UniquePtr<FizzBuzzTask>;

        type OptionalProbeTask;

        fn run(
            self: Pin<&mut OptionalProbeTask>,
            inputs: &mut OptionalProbeCxxInputs,
            outputs: &mut OptionalProbeCxxOutputs,
            context: &Context,
        );
        fn make_optional_probe_task() -> UniquePtr<OptionalProbeTask>;
    }

    extern "Rust" {
        type FizzBuzzCalculatorCxxInputs;

        /// Required input payload. Valid for the duration of `run`.
        unsafe fn integer(self: &FizzBuzzCalculatorCxxInputs) -> &u64;

        type FizzBuzzCalculatorCxxOutputs;

        /// Loaned output payload. Valid until `fizz_buzz_string_send`.
        unsafe fn fizz_buzz_string_value(self: &mut FizzBuzzCalculatorCxxOutputs) -> &mut String;
        /// Publishes the output. Dropping without sending discards it.
        fn fizz_buzz_string_send(self: &mut FizzBuzzCalculatorCxxOutputs);

        type OptionalProbeCxxInputs;

        fn value_has_value(self: &OptionalProbeCxxInputs) -> bool;
        fn value_len(self: &OptionalProbeCxxInputs) -> usize;
        unsafe fn value_value(self: &OptionalProbeCxxInputs) -> &u64;
        unsafe fn value_at(self: &OptionalProbeCxxInputs, index: usize) -> &u64;
        fn value_pop(self: &mut OptionalProbeCxxInputs);

        type OptionalProbeCxxOutputs;

        unsafe fn report_value(self: &mut OptionalProbeCxxOutputs) -> &mut String;
        fn report_send(self: &mut OptionalProbeCxxOutputs);
    }
}

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
        value: OptionalInput<u64> { channel: "optional_probe_in", capacity: 4, trigger: true },
    },
    publishers: {
        report: Output<String> { channel: "optional_probe_out" },
    },
}

#[cfg(all(test, not(miri)))]
mod tests {
    use std::time::Duration;

    use task_graph::CallbackSchedule;
    use test_tasks::{IncrementingIntegerPublisher, StringCollector, fizz_buzz};
    use testing::UnitTestExecutorBuilder;

    use super::{FIZZ_BUZZ_STRING_CHANNEL, FizzBuzzCalculator, INTEGER_CHANNEL, OptionalProbe};

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
    fn cxx_optional_input_queue_semantics() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("probe", OptionalProbe::new(), Duration::from_millis(1));
        let mut input = builder.add_test_publisher::<u64>("optional_probe_in");
        let output = builder.add_test_subscriber::<String>("optional_probe_out");
        let mut reports = Vec::new();
        builder.run(|mut executor| {
            input.send(7);
            input.send(9);
            for _ in 0..3 {
                executor.step();
                output.messages(&mut executor, |_, message| {
                    reports.push(message.message.clone());
                });
            }
        });
        assert_eq!(reports.first().map(String::as_str), Some("7:2:7"));
        assert!(reports.len() <= 2);
        if let Some(second) = reports.get(1) {
            assert_eq!(second, "9:1:9");
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
